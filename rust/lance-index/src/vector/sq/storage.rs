// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

use std::ops::Range;

use arrow::datatypes::Float64Type;
use arrow::{compute::concat_batches, datatypes::Float16Type};
use arrow_array::{
    cast::AsArray,
    types::{Float32Type, UInt64Type, UInt8Type},
    ArrayRef, RecordBatch, UInt64Array, UInt8Array,
};
use arrow_schema::{DataType, SchemaRef};
use async_trait::async_trait;
use deepsize::DeepSizeOf;
use lance_core::{Error, Result, ROW_ID};
use lance_file::reader::FileReader;
use lance_io::object_store::ObjectStore;
use lance_linalg::distance::{dot_distance, l2_distance_uint_scalar, DistanceType};
use lance_table::format::SelfDescribingFileReader;
use object_store::path::Path;
use serde::{Deserialize, Serialize};
use snafu::location;
use std::sync::Arc;

use super::{inverse_scalar_dist, scale_to_u8, ScalarQuantizer};
use crate::frag_reuse::FragReuseIndex;
use crate::{
    vector::{
        quantizer::{QuantizerMetadata, QuantizerStorage},
        storage::{DistCalculator, VectorStore},
        transform::Transformer,
        SQ_CODE_COLUMN,
    },
    IndexMetadata, INDEX_METADATA_SCHEMA_KEY,
};

pub const SQ_METADATA_KEY: &str = "lance:sq";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScalarQuantizationMetadata {
    pub dim: usize,
    pub num_bits: u16,
    pub bounds: Range<f64>,
}

impl DeepSizeOf for ScalarQuantizationMetadata {
    fn deep_size_of_children(&self, _context: &mut deepsize::Context) -> usize {
        0
    }
}

#[async_trait]
impl QuantizerMetadata for ScalarQuantizationMetadata {
    async fn load(reader: &FileReader) -> Result<Self> {
        let metadata_str = reader
            .schema()
            .metadata
            .get(SQ_METADATA_KEY)
            .ok_or(Error::Index {
                message: format!(
                    "Reading SQ metadata: metadata key {} not found",
                    SQ_METADATA_KEY
                ),
                location: location!(),
            })?;
        serde_json::from_str(metadata_str).map_err(|_| Error::Index {
            message: format!("Failed to parse index metadata: {}", metadata_str),
            location: location!(),
        })
    }
}

/// An immutable chunk of ScalarQuantizationStorage.
#[derive(Debug, Clone)]
struct SQStorageChunk {
    batch: RecordBatch,

    dim: usize,

    // Helper fields, references to the batch
    // These fields share the `Arc` pointer to the columns in batch,
    // so it does not take more memory.
    row_ids: UInt64Array,
    sq_codes: UInt8Array,
}

impl SQStorageChunk {
    // Create a new chunk from a RecordBatch.
    fn new(batch: RecordBatch) -> Result<Self> {
        let row_ids = batch
            .column_by_name(ROW_ID)
            .ok_or(Error::Index {
                message: "Row ID column not found in the batch".to_owned(),
                location: location!(),
            })?
            .as_primitive::<UInt64Type>()
            .clone();
        let fsl = batch
            .column_by_name(SQ_CODE_COLUMN)
            .ok_or(Error::Index {
                message: "SQ code column not found in the batch".to_owned(),
                location: location!(),
            })?
            .as_fixed_size_list();
        let dim = fsl.value_length() as usize;
        let sq_codes = fsl
            .values()
            .as_primitive_opt::<UInt8Type>()
            .ok_or(Error::Index {
                message: "SQ code column is not FixedSizeList<u8>".to_owned(),
                location: location!(),
            })?
            .clone();
        Ok(Self {
            batch,
            dim,
            row_ids,
            sq_codes,
        })
    }

    /// Returns vector dimension
    fn dim(&self) -> usize {
        self.dim
    }

    fn len(&self) -> usize {
        self.row_ids.len()
    }

    fn schema(&self) -> &SchemaRef {
        self.batch.schema_ref()
    }

    #[inline]
    fn row_id(&self, id: u32) -> u64 {
        self.row_ids.value(id as usize)
    }

    /// Get a slice of SQ code for id
    #[inline]
    fn sq_code_slice(&self, id: u32) -> &[u8] {
        // assert!(id < self.len() as u32);
        &self.sq_codes.values()[id as usize * self.dim..(id + 1) as usize * self.dim]
    }
}

impl DeepSizeOf for SQStorageChunk {
    fn deep_size_of_children(&self, _context: &mut deepsize::Context) -> usize {
        self.batch.get_array_memory_size()
    }
}

#[derive(Debug, Clone)]
pub struct ScalarQuantizationStorage {
    quantizer: ScalarQuantizer,

    distance_type: DistanceType,

    /// Chunks of storage
    offsets: Vec<u32>,
    chunks: Vec<SQStorageChunk>,
}

impl DeepSizeOf for ScalarQuantizationStorage {
    fn deep_size_of_children(&self, context: &mut deepsize::Context) -> usize {
        self.chunks
            .iter()
            .map(|c| c.deep_size_of_children(context))
            .sum()
    }
}

const SQ_CHUNK_CAPACITY: usize = 1024;

impl ScalarQuantizationStorage {
    pub fn try_new(
        num_bits: u16,
        distance_type: DistanceType,
        bounds: Range<f64>,
        batches: impl IntoIterator<Item = RecordBatch>,
        frag_reuse_index: Option<Arc<FragReuseIndex>>,
    ) -> Result<Self> {
        let mut chunks = Vec::with_capacity(SQ_CHUNK_CAPACITY);
        let mut offsets = Vec::with_capacity(SQ_CHUNK_CAPACITY + 1);
        offsets.push(0);
        for mut batch in batches.into_iter() {
            if let Some(frag_reuse_index_ref) = frag_reuse_index.as_ref() {
                batch = frag_reuse_index_ref.remap_row_ids_record_batch(batch, 0)?
            }
            offsets.push(offsets.last().unwrap() + batch.num_rows() as u32);
            let chunk = SQStorageChunk::new(batch)?;
            chunks.push(chunk);
        }
        let quantizer = ScalarQuantizer::with_bounds(num_bits, chunks[0].dim(), bounds);

        Ok(Self {
            quantizer,
            distance_type,
            offsets,
            chunks,
        })
    }

    /// Get the chunk that covers the id.
    ///
    /// Returns:
    /// `(offset, chunk)`
    ///
    /// We did not check out of range in this call. But the out of range will
    /// panic once you access the data in the last [SQStorageChunk].
    fn chunk(&self, id: u32) -> (u32, &SQStorageChunk) {
        match self.offsets.binary_search(&id) {
            Ok(o) => (self.offsets[o], &self.chunks[o]),
            Err(o) => (self.offsets[o - 1], &self.chunks[o - 1]),
        }
    }

    pub async fn load(
        object_store: &ObjectStore,
        path: &Path,
        frag_reuse_index: Option<Arc<FragReuseIndex>>,
    ) -> Result<Self> {
        let reader = FileReader::try_new_self_described(object_store, path, None).await?;
        let schema = reader.schema();

        let metadata_str = schema
            .metadata
            .get(INDEX_METADATA_SCHEMA_KEY)
            .ok_or(Error::Index {
                message: format!(
                    "Reading SQ storage: index key {} not found",
                    INDEX_METADATA_SCHEMA_KEY
                ),
                location: location!(),
            })?;
        let index_metadata: IndexMetadata =
            serde_json::from_str(metadata_str).map_err(|_| Error::Index {
                message: format!("Failed to parse index metadata: {}", metadata_str),
                location: location!(),
            })?;
        let distance_type = DistanceType::try_from(index_metadata.distance_type.as_str())?;
        let metadata = ScalarQuantizationMetadata::load(&reader).await?;

        Self::load_partition(
            &reader,
            0..reader.len(),
            distance_type,
            &metadata,
            frag_reuse_index,
        )
        .await
    }

    fn optimize(self) -> Result<Self> {
        if self.len() <= SQ_CHUNK_CAPACITY {
            Ok(self)
        } else {
            let mut new = self.clone();
            let batch = concat_batches(
                self.chunks[0].schema(),
                self.chunks.iter().map(|c| &c.batch),
            )?;
            new.offsets = vec![0, batch.num_rows() as u32];
            new.chunks = vec![SQStorageChunk::new(batch)?];
            Ok(new)
        }
    }
}

#[async_trait]
impl QuantizerStorage for ScalarQuantizationStorage {
    type Metadata = ScalarQuantizationMetadata;

    fn try_from_batch(
        batch: RecordBatch,
        metadata: &Self::Metadata,
        distance_type: DistanceType,
        frag_reuse_index: Option<Arc<FragReuseIndex>>,
    ) -> Result<Self>
    where
        Self: Sized,
    {
        Self::try_new(
            metadata.num_bits,
            distance_type,
            metadata.bounds.clone(),
            [batch],
            frag_reuse_index,
        )
    }

    fn metadata(&self) -> &Self::Metadata {
        &self.quantizer.metadata
    }

    /// Load a partition of SQ storage from disk.
    ///
    /// Parameters
    /// ----------
    /// - *reader: file reader
    /// - *range: row range of the partition
    /// - *metric_type: metric type of the vectors
    /// - *metadata: scalar quantization metadata
    async fn load_partition(
        reader: &FileReader,
        range: std::ops::Range<usize>,
        distance_type: DistanceType,
        metadata: &Self::Metadata,
        frag_reuse_index: Option<Arc<FragReuseIndex>>,
    ) -> Result<Self> {
        let schema = reader.schema();
        let batch = reader.read_range(range, schema).await?;

        Self::try_new(
            metadata.num_bits,
            distance_type,
            metadata.bounds.clone(),
            [batch],
            frag_reuse_index,
        )
    }
}

impl VectorStore for ScalarQuantizationStorage {
    type DistanceCalculator<'a> = SQDistCalculator<'a>;

    fn to_batches(&self) -> Result<impl Iterator<Item = RecordBatch>> {
        Ok(self.chunks.iter().map(|c| c.batch.clone()))
    }

    fn append_batch(&self, batch: RecordBatch, vector_column: &str) -> Result<Self> {
        // TODO: use chunked storage
        let transformer = super::transform::SQTransformer::new(
            self.quantizer.clone(),
            vector_column.to_string(),
            SQ_CODE_COLUMN.to_string(),
        );

        let new_batch = transformer.transform(&batch)?;

        // self.quantizer.transform(data)
        let mut storage = self.clone();
        let offset = self.len() as u32;
        let new_chunk = SQStorageChunk::new(new_batch)?;
        storage.offsets.push(offset + new_chunk.len() as u32);
        storage.chunks.push(new_chunk);

        storage.optimize()
    }

    fn schema(&self) -> &SchemaRef {
        self.chunks[0].schema()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn len(&self) -> usize {
        *self.offsets.last().unwrap() as usize
    }

    /// Return the [DistanceType] of the vectors.
    fn distance_type(&self) -> DistanceType {
        self.distance_type
    }

    fn row_id(&self, id: u32) -> u64 {
        let (offset, chunk) = self.chunk(id);
        chunk.row_id(id - offset)
    }

    fn row_ids(&self) -> impl Iterator<Item = &u64> {
        self.chunks.iter().flat_map(|c| c.row_ids.values())
    }

    /// Create a [DistCalculator] to compute the distance between the query.
    ///
    /// Using dist calculator can be more efficient as it can pre-compute some
    /// values.
    fn dist_calculator(&self, query: ArrayRef) -> Self::DistanceCalculator<'_> {
        SQDistCalculator::new(query, self, self.quantizer.bounds())
    }

    fn dist_calculator_from_id(&self, id: u32) -> Self::DistanceCalculator<'_> {
        let (offset, chunk) = self.chunk(id);
        let query_sq_code = chunk.sq_code_slice(id - offset).to_vec();
        SQDistCalculator {
            query_sq_code,
            bounds: self.quantizer.bounds(),
            storage: self,
        }
    }
}

pub struct SQDistCalculator<'a> {
    query_sq_code: Vec<u8>,
    bounds: Range<f64>,
    storage: &'a ScalarQuantizationStorage,
}

impl<'a> SQDistCalculator<'a> {
    fn new(query: ArrayRef, storage: &'a ScalarQuantizationStorage, bounds: Range<f64>) -> Self {
        // This is okay-ish to use hand-rolled dynamic dispatch here
        // since we search 10s-100s of partitions, we can afford the overhead
        // this could be annoying at indexing time for HNSW, which requires constructing the
        // dist calculator frequently. However, HNSW isn't first-class citizen in Lance yet. so be it.
        let query_sq_code = match query.data_type() {
            DataType::Float16 => {
                scale_to_u8::<Float16Type>(query.as_primitive::<Float16Type>().values(), &bounds)
            }
            DataType::Float32 => {
                scale_to_u8::<Float32Type>(query.as_primitive::<Float32Type>().values(), &bounds)
            }
            DataType::Float64 => {
                scale_to_u8::<Float64Type>(query.as_primitive::<Float64Type>().values(), &bounds)
            }
            _ => {
                panic!("Unsupported data type for ScalarQuantizationStorage");
            }
        };
        Self {
            query_sq_code,
            bounds,
            storage,
        }
    }
}

impl DistCalculator for SQDistCalculator<'_> {
    fn distance(&self, id: u32) -> f32 {
        let (offset, chunk) = self.storage.chunk(id);
        let sq_code = chunk.sq_code_slice(id - offset);
        let dist = match self.storage.distance_type {
            DistanceType::L2 | DistanceType::Cosine => {
                l2_distance_uint_scalar(sq_code, &self.query_sq_code)
            }
            DistanceType::Dot => dot_distance(sq_code, &self.query_sq_code),
            _ => panic!("We should not reach here: sq distance can only be L2 or Dot"),
        };
        inverse_scalar_dist(std::iter::once(dist), &self.bounds)[0]
    }

    fn distance_all(&self, _k_hint: usize) -> Vec<f32> {
        match self.storage.distance_type {
            DistanceType::L2 | DistanceType::Cosine => inverse_scalar_dist(
                self.storage.chunks.iter().flat_map(|c| {
                    c.sq_codes
                        .values()
                        .chunks_exact(c.dim())
                        .map(|sq_codes| l2_distance_uint_scalar(sq_codes, &self.query_sq_code))
                }),
                &self.bounds,
            ),
            DistanceType::Dot => inverse_scalar_dist(
                self.storage.chunks.iter().flat_map(|c| {
                    c.sq_codes
                        .values()
                        .chunks_exact(c.dim())
                        .map(|sq_codes| dot_distance(sq_codes, &self.query_sq_code))
                }),
                &self.bounds,
            ),
            _ => panic!("We should not reach here: sq distance can only be L2 or Dot"),
        }
    }

    #[allow(unused_variables)]
    fn prefetch(&self, id: u32) {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            const CACHE_LINE_SIZE: usize = 64;

            let (offset, chunk) = self.storage.chunk(id);
            let dim = chunk.dim();
            let base_ptr = chunk.sq_code_slice(id - offset).as_ptr();

            unsafe {
                // Loop over the sq_code to prefetch each cache line
                for offset in (0..dim).step_by(CACHE_LINE_SIZE) {
                    {
                        use core::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
                        _mm_prefetch(base_ptr.add(offset) as *const i8, _MM_HINT_T0);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::iter::repeat_with;
    use std::sync::Arc;

    use arrow_array::FixedSizeListArray;
    use arrow_schema::{DataType, Field, Schema};
    use lance_arrow::FixedSizeListArrayExt;
    use lance_testing::datagen::generate_random_array;
    use rand::prelude::*;

    fn create_record_batch(row_ids: Range<u64>) -> RecordBatch {
        const DIM: usize = 64;

        let mut rng = rand::thread_rng();
        let row_ids = UInt64Array::from_iter_values(row_ids);
        let sq_code =
            UInt8Array::from_iter_values(repeat_with(|| rng.gen::<u8>()).take(row_ids.len() * DIM));
        let code_arr = FixedSizeListArray::try_new_from_values(sq_code, DIM as i32).unwrap();

        let schema = Arc::new(Schema::new(vec![
            Field::new(ROW_ID, DataType::UInt64, false),
            Field::new(
                SQ_CODE_COLUMN,
                DataType::FixedSizeList(
                    Arc::new(Field::new("item", DataType::UInt8, true)),
                    DIM as i32,
                ),
                false,
            ),
        ]));
        RecordBatch::try_new(schema, vec![Arc::new(row_ids), Arc::new(code_arr)]).unwrap()
    }

    #[test]
    fn test_get_chunks() {
        const DIM: usize = 64;

        let storage = ScalarQuantizationStorage::try_new(
            8,
            DistanceType::L2,
            -0.7..0.7,
            (0..4).map(|start| create_record_batch(start * 100..(start + 1) * 100)),
            None,
        )
        .unwrap();

        assert_eq!(storage.len(), 400);

        let (offset, chunk) = storage.chunk(0);
        assert_eq!(offset, 0);
        assert_eq!(chunk.row_id(20), 20);

        let (offset, _) = storage.chunk(50);
        assert_eq!(offset, 0);

        let row_ids = UInt64Array::from_iter_values(100..250);
        let vector_data = generate_random_array(row_ids.len() * DIM);
        let fsl = FixedSizeListArray::try_new_from_values(vector_data, DIM as i32).unwrap();

        let schema = Arc::new(Schema::new(vec![
            Field::new(ROW_ID, DataType::UInt64, false),
            Field::new(
                "vector",
                DataType::FixedSizeList(
                    Arc::new(Field::new("item", DataType::Float32, true)),
                    DIM as i32,
                ),
                false,
            ),
        ]));

        let second_batch =
            RecordBatch::try_new(schema, vec![Arc::new(row_ids), Arc::new(fsl)]).unwrap();
        let storage = storage.append_batch(second_batch, "vector").unwrap();

        assert_eq!(storage.len(), 550);
        let (offset, chunk) = storage.chunk(112);
        assert_eq!(offset, 100);
        assert_eq!(chunk.row_id(10), 110);

        let (offset, chunk) = storage.chunk(432);
        assert_eq!(offset, 400);
        assert_eq!(chunk.row_id(5), 105);
    }
}
