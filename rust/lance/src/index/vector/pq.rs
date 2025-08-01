// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

use std::sync::Arc;
use std::{any::Any, collections::HashMap};

use arrow::compute::concat;
use arrow_array::types::UInt64Type;
use arrow_array::{
    cast::{as_primitive_array, AsArray},
    Array, FixedSizeListArray, RecordBatch, UInt64Array, UInt8Array,
};
use arrow_array::{ArrayRef, Float32Array, UInt32Array};
use arrow_ord::sort::sort_to_indices;
use arrow_schema::{DataType, Field, Schema};
use arrow_select::take::take;
use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use deepsize::DeepSizeOf;
use lance_core::utils::address::RowAddress;
use lance_core::utils::tokio::spawn_cpu;
use lance_core::{ROW_ID, ROW_ID_FIELD};
use lance_index::frag_reuse::FragReuseIndex;
use lance_index::metrics::MetricsCollector;
use lance_index::vector::ivf::storage::IvfModel;
use lance_index::vector::pq::storage::{transpose, ProductQuantizationStorage};
use lance_index::vector::quantizer::{Quantization, QuantizationType, Quantizer};
use lance_index::vector::v3::subindex::SubIndexType;
use lance_index::{
    vector::{pq::ProductQuantizer, Query},
    Index, IndexType,
};
use lance_io::{traits::Reader, utils::read_fixed_stride_array};
use lance_linalg::distance::{DistanceType, MetricType};
use log::{info, warn};
use roaring::RoaringBitmap;
use serde_json::json;
use snafu::location;
use tracing::{instrument, span, Level};
// Re-export
pub use lance_index::vector::pq::PQBuildParams;
use lance_linalg::kernels::normalize_fsl;

use super::VectorIndex;
use crate::index::prefilter::PreFilter;
use crate::index::vector::utils::maybe_sample_training_data;
use crate::io::exec::knn::KNN_INDEX_SCHEMA;
use crate::{arrow::*, Dataset};
use crate::{Error, Result};

/// Product Quantization Index.
///
#[derive(Clone)]
pub struct PQIndex {
    /// Product quantizer.
    pub pq: ProductQuantizer,

    /// PQ code
    /// the PQ codes are stored in a transposed way,
    /// call `Self::get_pq_codes` to get the PQ code for a specific vector.
    pub code: Option<Arc<UInt8Array>>,

    /// ROW Id used to refer to the actual row in dataset.
    pub row_ids: Option<Arc<UInt64Array>>,

    /// Metric type.
    metric_type: MetricType,

    frag_reuse_index: Option<Arc<FragReuseIndex>>,
}

impl DeepSizeOf for PQIndex {
    fn deep_size_of_children(&self, context: &mut deepsize::Context) -> usize {
        self.pq.deep_size_of_children(context)
            + self
                .code
                .as_ref()
                .map(|code| code.get_array_memory_size())
                .unwrap_or(0)
            + self
                .row_ids
                .as_ref()
                .map(|row_ids| row_ids.get_array_memory_size())
                .unwrap_or(0)
    }
}

impl std::fmt::Debug for PQIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PQ(m={}, nbits={}, {})",
            self.pq.code_dim(),
            self.pq.num_bits,
            self.metric_type
        )
    }
}

impl PQIndex {
    /// Load a PQ index (page) from the disk.
    pub(crate) fn new(
        pq: ProductQuantizer,
        metric_type: MetricType,
        frag_reuse_index: Option<Arc<FragReuseIndex>>,
    ) -> Self {
        Self {
            code: None,
            row_ids: None,
            pq,
            metric_type,
            frag_reuse_index,
        }
    }

    /// Filter the row id and PQ code arrays based on the pre-filter.
    fn filter_arrays(
        pre_filter: &dyn PreFilter,
        code: Arc<UInt8Array>,
        row_ids: Arc<UInt64Array>,
        _num_sub_vectors: i32,
    ) -> Result<(Arc<UInt8Array>, Arc<UInt64Array>)> {
        let num_vectors = row_ids.len();
        if num_vectors == 0 {
            warn!("Filtering on empty PQ code array");
            return Ok((code, row_ids));
        }
        let indices_to_keep = pre_filter.filter_row_ids(Box::new(row_ids.values().iter()));
        let indices_to_keep = UInt64Array::from(indices_to_keep);

        let row_ids = take(row_ids.as_ref(), &indices_to_keep, None)?;
        let row_ids = Arc::new(as_primitive_array(&row_ids).clone());

        let code = code
            .values()
            .chunks_exact(num_vectors)
            .flat_map(|c| {
                let mut filtered = Vec::with_capacity(indices_to_keep.len());
                for idx in indices_to_keep.values() {
                    filtered.push(c[*idx as usize]);
                }
                filtered
            })
            .collect();

        Ok((Arc::new(code), row_ids))
    }

    fn get_pq_codes(transposed_codes: &UInt8Array, vec_idx: usize, num_vectors: usize) -> Vec<u8> {
        transposed_codes
            .values()
            .iter()
            .skip(vec_idx)
            .step_by(num_vectors)
            .cloned()
            .collect()
    }
}

#[async_trait]
impl Index for PQIndex {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_index(self: Arc<Self>) -> Arc<dyn Index> {
        self
    }

    fn as_vector_index(self: Arc<Self>) -> Result<Arc<dyn VectorIndex>> {
        Ok(self)
    }

    fn index_type(&self) -> IndexType {
        IndexType::Vector
    }

    async fn prewarm(&self) -> Result<()> {
        // TODO: Investigate
        Ok(())
    }

    fn statistics(&self) -> Result<serde_json::Value> {
        Ok(json!({
            "index_type": "PQ",
            "nbits": self.pq.num_bits,
            "num_sub_vectors": self.pq.code_dim(),
            "dimension": self.pq.dimension,
            "metric_type": self.metric_type.to_string(),
        }))
    }

    async fn calculate_included_frags(&self) -> Result<RoaringBitmap> {
        if let Some(row_ids) = &self.row_ids {
            let mut frag_ids = row_ids
                .values()
                .iter()
                .map(|&row_id| RowAddress::from(row_id).fragment_id())
                .collect::<Vec<_>>();
            frag_ids.sort();
            frag_ids.dedup();
            Ok(RoaringBitmap::from_sorted_iter(frag_ids).unwrap())
        } else {
            Err(Error::Index {
                message: "PQIndex::calculate_included_frags: PQ is not initialized".to_string(),
                location: location!(),
            })
        }
    }
}

#[async_trait]
impl VectorIndex for PQIndex {
    /// Search top-k nearest neighbors for `key` within one PQ partition.
    ///
    #[instrument(level = "debug", skip_all, name = "PQIndex::search")]
    async fn search(
        &self,
        query: &Query,
        pre_filter: Arc<dyn PreFilter>,
        metrics: &dyn MetricsCollector,
    ) -> Result<RecordBatch> {
        if self.code.is_none() || self.row_ids.is_none() {
            return Err(Error::Index {
                message: "PQIndex::search: PQ is not initialized".to_string(),
                location: location!(),
            });
        }
        pre_filter.wait_for_ready().await?;

        let code = self.code.as_ref().unwrap().clone();
        let row_ids = self.row_ids.as_ref().unwrap().clone();

        metrics.record_comparisons(row_ids.len());

        let pq = self.pq.clone();
        let query = query.clone();
        let num_sub_vectors = self.pq.code_dim() as i32;
        spawn_cpu(move || {
            let (code, row_ids) = if pre_filter.is_empty() {
                Ok((code, row_ids))
            } else {
                Self::filter_arrays(pre_filter.as_ref(), code, row_ids, num_sub_vectors)
            }?;

            // Pre-compute distance table for each sub-vector.
            let distances = pq.compute_distances(query.key.as_ref(), &code)?;

            debug_assert_eq!(distances.len(), row_ids.len());

            let limit = query.k * query.refine_factor.unwrap_or(1) as usize;
            if query.lower_bound.is_none() && query.upper_bound.is_none() {
                let indices = sort_to_indices(&distances, None, Some(limit))?;
                let distances = take(&distances, &indices, None)?;
                let row_ids = take(row_ids.as_ref(), &indices, None)?;
                Ok(RecordBatch::try_new(
                    KNN_INDEX_SCHEMA.clone(),
                    vec![distances, row_ids],
                )?)
            } else {
                let indices = sort_to_indices(&distances, None, None)?;
                let mut dists = Vec::with_capacity(limit);
                let mut ids = Vec::with_capacity(limit);
                for idx in indices.values().iter() {
                    let dist = distances.value(*idx as usize);
                    let id = row_ids.value(*idx as usize);
                    if query.lower_bound.is_some_and(|lb| dist < lb) {
                        continue;
                    }
                    if query.upper_bound.is_some_and(|ub| dist >= ub) {
                        break;
                    }

                    dists.push(dist);
                    ids.push(id);

                    if dists.len() >= limit {
                        break;
                    }
                }
                let dists = Arc::new(Float32Array::from(dists));
                let ids = Arc::new(UInt64Array::from(ids));
                Ok(RecordBatch::try_new(
                    KNN_INDEX_SCHEMA.clone(),
                    vec![dists, ids],
                )?)
            }
        })
        .await
    }

    fn find_partitions(&self, _: &Query) -> Result<UInt32Array> {
        unimplemented!("only for IVF")
    }

    fn total_partitions(&self) -> usize {
        1
    }

    async fn search_in_partition(
        &self,
        _: usize,
        _: &Query,
        _: Arc<dyn PreFilter>,
        _: &dyn MetricsCollector,
    ) -> Result<RecordBatch> {
        unimplemented!("only for IVF")
    }

    fn is_loadable(&self) -> bool {
        true
    }

    fn use_residual(&self) -> bool {
        ProductQuantizer::use_residual(self.metric_type)
    }

    /// Load a PQ index (page) from the disk.
    async fn load(
        &self,
        reader: Arc<dyn Reader>,
        offset: usize,
        length: usize,
    ) -> Result<Box<dyn VectorIndex>> {
        let pq_code_length = self.pq.code_dim() * length;
        let pq_codes = read_fixed_stride_array(
            reader.as_ref(),
            &DataType::UInt8,
            offset,
            pq_code_length,
            ..,
        )
        .await?;

        let row_id_offset = offset + pq_code_length /* *1 */;
        let row_ids = read_fixed_stride_array(
            reader.as_ref(),
            &DataType::UInt64,
            row_id_offset,
            length,
            ..,
        )
        .await?;

        let pq_codes = transpose(
            pq_codes.as_primitive(),
            row_ids.len(),
            self.pq.num_sub_vectors,
        );

        let (primitive_row_ids, transposed_pq_codes) =
            if let Some(frag_reuse_index_ref) = self.frag_reuse_index.as_ref() {
                let num_vectors = row_ids.len();
                let row_ids = row_ids.as_primitive::<UInt64Type>().values().iter();
                let (remapped_row_ids, remapped_pq_codes): (Vec<u64>, Vec<Vec<u8>>) = row_ids
                    .enumerate()
                    .filter_map(|(vec_idx, old_row_id)| {
                        let new_row_id = frag_reuse_index_ref.remap_row_id(*old_row_id);
                        new_row_id.map(|new_row_id| {
                            (
                                new_row_id,
                                Self::get_pq_codes(&pq_codes, vec_idx, num_vectors),
                            )
                        })
                    })
                    .unzip();
                let transposed_codes = transpose(
                    &UInt8Array::from_iter_values(remapped_pq_codes.into_iter().flatten()),
                    remapped_row_ids.len(),
                    self.pq.num_sub_vectors,
                );
                (
                    Arc::new(UInt64Array::from_iter_values(remapped_row_ids)),
                    Arc::new(transposed_codes),
                )
            } else {
                (Arc::new(row_ids.as_primitive().clone()), Arc::new(pq_codes))
            };

        Ok(Box::new(Self {
            code: Some(transposed_pq_codes),
            row_ids: Some(primitive_row_ids),
            pq: self.pq.clone(),
            metric_type: self.metric_type,
            frag_reuse_index: self.frag_reuse_index.clone(),
        }))
    }

    async fn to_batch_stream(&self, with_vector: bool) -> Result<SendableRecordBatchStream> {
        let row_ids = self.row_ids.clone().ok_or(Error::Index {
            message: "PQIndex::to_batch_stream: row ids not loaded for PQ".to_string(),
            location: location!(),
        })?;

        let num_rows = row_ids.len();
        let mut fields = vec![ROW_ID_FIELD.clone()];
        let mut columns: Vec<ArrayRef> = vec![row_ids];
        if with_vector {
            let transposed_codes = self.code.clone().ok_or(Error::Index {
                message: "PQIndex::to_batch_stream: PQ codes not loaded for PQ".to_string(),
                location: location!(),
            })?;
            let original_codes = transpose(&transposed_codes, self.pq.num_sub_vectors, num_rows);
            fields.push(Field::new(
                self.pq.column(),
                DataType::FixedSizeList(
                    Arc::new(Field::new("item", DataType::UInt8, true)),
                    self.pq.code_dim() as i32,
                ),
                true,
            ));
            columns.push(Arc::new(FixedSizeListArray::try_new_from_values(
                original_codes,
                self.pq.code_dim() as i32,
            )?));
        }

        let batch = RecordBatch::try_new(Arc::new(Schema::new(fields)), columns)?;
        let stream = RecordBatchStreamAdapter::new(
            batch.schema(),
            futures::stream::once(futures::future::ready(Ok(batch))),
        );
        Ok(Box::pin(stream))
    }

    fn num_rows(&self) -> u64 {
        self.row_ids
            .as_ref()
            .map_or(0, |row_ids| row_ids.len() as u64)
    }

    fn row_ids(&self) -> Box<dyn Iterator<Item = &u64>> {
        todo!("this method is for only IVF_HNSW_* index");
    }

    async fn remap(&mut self, mapping: &HashMap<u64, Option<u64>>) -> Result<()> {
        let num_vectors = self.row_ids.as_ref().unwrap().len();
        let row_ids = self.row_ids.as_ref().unwrap().values().iter();
        let transposed_codes = self.code.as_ref().unwrap();
        let remapped = row_ids
            .enumerate()
            .filter_map(|(vec_idx, old_row_id)| {
                let new_row_id = mapping.get(old_row_id).cloned();
                // If the row id is not in the mapping then this row is not remapped and we keep as is
                let new_row_id = new_row_id.unwrap_or(Some(*old_row_id));
                new_row_id.map(|new_row_id| {
                    (
                        new_row_id,
                        Self::get_pq_codes(transposed_codes, vec_idx, num_vectors),
                    )
                })
            })
            .collect::<Vec<_>>();

        self.row_ids = Some(Arc::new(UInt64Array::from_iter_values(
            remapped.iter().map(|(row_id, _)| *row_id),
        )));

        let pq_codes =
            UInt8Array::from_iter_values(remapped.into_iter().flat_map(|(_, code)| code));
        let transposed_codes = transpose(
            &pq_codes,
            self.row_ids.as_ref().unwrap().len(),
            self.pq.num_sub_vectors,
        );
        self.code = Some(Arc::new(transposed_codes));
        Ok(())
    }

    fn ivf_model(&self) -> &IvfModel {
        unimplemented!("only for IVF")
    }
    fn quantizer(&self) -> Quantizer {
        unimplemented!("only for IVF")
    }

    /// the index type of this vector index.
    fn sub_index_type(&self) -> (SubIndexType, QuantizationType) {
        (SubIndexType::Flat, QuantizationType::Product)
    }

    fn metric_type(&self) -> MetricType {
        self.metric_type
    }
}

/// Train Product Quantizer model.
///
/// Parameters:
/// - `dataset`: The dataset to train the PQ model.
/// - `column`: The column name of the dataset.
/// - `dim`: The dimension of the vectors.
/// - `metric_type`: The metric type of the vectors.
/// - `params`: The parameters to train the PQ model.
/// - `ivf`: If provided, the IVF model to compute the residual for PQ training.
pub async fn build_pq_model(
    dataset: &Dataset,
    column: &str,
    dim: usize,
    metric_type: MetricType,
    params: &PQBuildParams,
    ivf: Option<&IvfModel>,
) -> Result<ProductQuantizer> {
    if let Some(codebook) = &params.codebook {
        let dt = if metric_type == MetricType::Cosine {
            info!("Normalize training data for PQ training: Cosine");
            MetricType::L2
        } else {
            metric_type
        };

        return match codebook.data_type() {
            DataType::Float16 | DataType::Float32 | DataType::Float64 => Ok(ProductQuantizer::new(
                params.num_sub_vectors,
                params.num_bits as u32,
                dim,
                FixedSizeListArray::try_new_from_values(
                    codebook.slice(0, codebook.len()),
                    dim as i32,
                )?,
                dt,
            )),
            _ => Err(Error::Index {
                message: format!("Wrong codebook data type: {:?}", codebook.data_type()),
                location: location!(),
            }),
        };
    }
    info!(
        "Start to train PQ code: PQ{}, bits={}",
        params.num_sub_vectors, params.num_bits
    );
    let expected_sample_size =
        lance_index::vector::pq::num_centroids(params.num_bits as u32) * params.sample_rate;
    info!(
        "Loading training data for PQ. Sample size: {}",
        expected_sample_size
    );
    let start = std::time::Instant::now();
    let mut training_data =
        maybe_sample_training_data(dataset, column, expected_sample_size).await?;
    info!(
        "Finished loading training data in {:02} seconds",
        start.elapsed().as_secs_f32()
    );
    assert_eq!(training_data.logical_null_count(), 0);

    info!(
        "starting to compute partitions for PQ training, sample size: {}",
        training_data.len()
    );

    if metric_type == MetricType::Cosine {
        info!("Normalize training data for PQ training: Cosine");
        training_data = normalize_fsl(&training_data)?;
    }

    let training_data = if let Some(ivf) = ivf {
        // Compute residual for PQ training.
        //
        // TODO: consolidate IVF models to `lance_index`.
        let ivf2 = lance_index::vector::ivf::new_ivf_transformer(
            ivf.centroids.clone().unwrap(),
            MetricType::L2,
            vec![],
        );
        span!(Level::INFO, "compute residual for PQ training")
            .in_scope(|| ivf2.compute_residual(&training_data))?
    } else {
        training_data
    };

    let num_codes = 2_usize.pow(params.num_bits as u32);
    if training_data.len() < num_codes {
        return Err(Error::Index {
            message: format!(
                "Not enough rows to train PQ. Requires {:?} rows but only {:?} available",
                num_codes,
                training_data.len()
            ),
            location: location!(),
        });
    }

    info!("Start train PQ: params={:#?}", params);
    let pq = ProductQuantizer::build(&training_data, DistanceType::L2, params)?;
    info!("Trained PQ in: {} seconds", start.elapsed().as_secs_f32());
    Ok(pq)
}

pub(crate) fn build_pq_storage(
    distance_type: DistanceType,
    row_ids: Arc<dyn Array>,
    code_array: Vec<Arc<dyn Array>>,
    pq: ProductQuantizer,
) -> Result<ProductQuantizationStorage> {
    let pq_arrs = code_array.iter().map(|a| a.as_ref()).collect::<Vec<_>>();
    let pq_column = concat(&pq_arrs)?;
    std::mem::drop(code_array);

    let pq_batch = RecordBatch::try_from_iter_with_nullable(vec![
        (ROW_ID, row_ids, true),
        (pq.column(), pq_column, false),
    ])?;
    let pq_store = ProductQuantizationStorage::new(
        pq.codebook.clone(),
        pq_batch,
        pq.num_bits,
        pq.code_dim(),
        pq.dimension,
        distance_type,
        false,
        // TODO: support auto-remap with frag_reuse_index for HNSW
        None,
    )?;

    Ok(pq_store)
}
#[cfg(test)]
mod tests {
    use super::*;

    use std::ops::Range;

    use arrow::datatypes::Float32Type;
    use arrow_array::RecordBatchIterator;
    use arrow_schema::{Field, Schema};
    use tempfile::tempdir;

    use crate::index::vector::ivf::build_ivf_model;
    use lance_core::utils::mask::RowIdMask;
    use lance_index::vector::ivf::IvfBuildParams;
    use lance_testing::datagen::generate_random_array_with_range;

    const DIM: usize = 128;
    async fn generate_dataset(
        test_uri: &str,
        range: Range<f32>,
    ) -> (Dataset, Arc<FixedSizeListArray>) {
        let vectors = generate_random_array_with_range::<Float32Type>(1000 * DIM, range);
        let metadata: HashMap<String, String> = vec![("test".to_string(), "ivf_pq".to_string())]
            .into_iter()
            .collect();

        let schema = Arc::new(
            Schema::new(vec![Field::new(
                "vector",
                DataType::FixedSizeList(
                    Arc::new(Field::new("item", DataType::Float32, true)),
                    DIM as i32,
                ),
                true,
            )])
            .with_metadata(metadata),
        );
        let fsl = Arc::new(FixedSizeListArray::try_new_from_values(vectors, DIM as i32).unwrap());
        let batch = RecordBatch::try_new(schema.clone(), vec![fsl.clone()]).unwrap();

        let batches = RecordBatchIterator::new(vec![batch].into_iter().map(Ok), schema.clone());
        (Dataset::write(batches, test_uri, None).await.unwrap(), fsl)
    }

    #[tokio::test]
    async fn test_build_pq_model_l2() {
        let test_dir = tempdir().unwrap();
        let test_uri = test_dir.path().to_str().unwrap();

        let (dataset, _) = generate_dataset(test_uri, 100.0..120.0).await;

        let centroids = generate_random_array_with_range::<Float32Type>(4 * DIM, -1.0..1.0);
        let fsl = FixedSizeListArray::try_new_from_values(centroids, DIM as i32).unwrap();
        let ivf = IvfModel::new(fsl, None);
        let params = PQBuildParams::new(16, 8);
        let pq = build_pq_model(&dataset, "vector", DIM, MetricType::L2, &params, Some(&ivf))
            .await
            .unwrap();

        assert_eq!(pq.code_dim(), 16);
        assert_eq!(pq.num_bits, 8);
        assert_eq!(pq.dimension, DIM);

        let codebook = pq.codebook;
        assert_eq!(codebook.len(), 256);
        codebook
            .values()
            .as_primitive::<Float32Type>()
            .values()
            .iter()
            .for_each(|v| {
                assert!((99.0..121.0).contains(v));
            });
    }

    #[tokio::test]
    async fn test_build_pq_model_cosine() {
        let test_dir = tempdir().unwrap();
        let test_uri = test_dir.path().to_str().unwrap();

        let (dataset, vectors) = generate_dataset(test_uri, 100.0..120.0).await;

        let ivf_params = IvfBuildParams::new(4);
        let ivf = build_ivf_model(&dataset, "vector", DIM, MetricType::Cosine, &ivf_params)
            .await
            .unwrap();
        let params = PQBuildParams::new(16, 8);
        let pq = build_pq_model(
            &dataset,
            "vector",
            DIM,
            MetricType::Cosine,
            &params,
            Some(&ivf),
        )
        .await
        .unwrap();

        assert_eq!(pq.code_dim(), 16);
        assert_eq!(pq.num_bits, 8);
        assert_eq!(pq.dimension, DIM);

        #[allow(clippy::redundant_clone)]
        let codebook = pq.codebook.clone();
        assert_eq!(codebook.len(), 256);
        codebook
            .values()
            .as_primitive::<Float32Type>()
            .values()
            .iter()
            .for_each(|v| {
                assert!((-1.0..1.0).contains(v));
            });

        let vectors = normalize_fsl(&vectors).unwrap();
        let row = vectors.slice(0, 1);

        let ivf2 = lance_index::vector::ivf::new_ivf_transformer(
            ivf.centroids.clone().unwrap(),
            MetricType::L2,
            vec![],
        );

        let residual_query = ivf2.compute_residual(&row).unwrap();
        let pq_code = pq.quantize(&residual_query).unwrap();
        let distances = pq
            .compute_distances(
                &residual_query.value(0),
                pq_code.as_fixed_size_list().values().as_primitive(),
            )
            .unwrap();
        assert!(
            distances.values().iter().all(|&d| d <= 0.001),
            "distances: {:?}",
            distances
        );
    }

    struct TestPreFilter {
        row_ids: Vec<u64>,
    }

    impl TestPreFilter {
        fn new(row_ids: Vec<u64>) -> Self {
            Self { row_ids }
        }
    }

    #[async_trait]
    impl PreFilter for TestPreFilter {
        async fn wait_for_ready(&self) -> Result<()> {
            Ok(())
        }

        fn is_empty(&self) -> bool {
            self.row_ids.is_empty()
        }

        fn mask(&self) -> Arc<RowIdMask> {
            RowIdMask::all_rows().into()
        }

        fn filter_row_ids<'a>(&self, row_ids: Box<dyn Iterator<Item = &'a u64> + 'a>) -> Vec<u64> {
            row_ids
                .filter(|&row_id| self.row_ids.contains(row_id))
                .cloned()
                .collect()
        }
    }

    #[test]
    fn test_filter_on_empty_pq_code() {
        let pre_filter = TestPreFilter::new(vec![1, 3, 5, 7, 9]);
        let code = Arc::new(UInt8Array::from(Vec::<u8>::new()));
        let row_ids = Arc::new(UInt64Array::from(Vec::<u64>::new()));

        let (code, row_ids) = PQIndex::filter_arrays(&pre_filter, code, row_ids, 16).unwrap();
        assert!(code.values().is_empty());
        assert!(row_ids.is_empty());
    }
}
