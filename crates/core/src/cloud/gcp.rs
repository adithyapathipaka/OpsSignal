//! Google Cloud: GCS objects.
//!
//! Credentials come from object_store's GCP chain:
//! `GOOGLE_SERVICE_ACCOUNT` / `GOOGLE_APPLICATION_CREDENTIALS`, gcloud
//! application-default credentials, then the metadata server (GKE
//! workload identity, GCE service accounts).

use super::{config_err, fetch_object, ReqwestConnector};
use crate::signal::SignalError;

pub(crate) async fn fetch_gcs(bucket: &str, object: &str) -> Result<String, SignalError> {
    let store = object_store::gcp::GoogleCloudStorageBuilder::from_env()
        .with_bucket_name(bucket)
        .with_http_connector(ReqwestConnector)
        .build()
        .map_err(|e| config_err(e.to_string()))?;
    fetch_object(store, object).await
}
