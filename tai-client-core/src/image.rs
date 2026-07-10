use crate::error::ClientError;
use std::collections::HashMap;
use tai_proto::ImageMetadata;
use tracing::{debug, warn};

#[derive(Debug, Clone)]
pub struct PendingImage {
    metadata: ImageMetadata,
    data: Vec<u8>,
}

impl PendingImage {
    fn new(metadata: ImageMetadata) -> Result<Self, ClientError> {
        let capacity =
            usize::try_from(metadata.byte_len).map_err(|_| ClientError::ImageTooLarge)?;
        Ok(Self {
            metadata,
            data: Vec::with_capacity(capacity),
        })
    }

    fn push_chunk(&mut self, chunk: &[u8]) -> Result<(), ClientError> {
        let expected =
            usize::try_from(self.metadata.byte_len).map_err(|_| ClientError::ImageTooLarge)?;
        let next_len = self.data.len().saturating_add(chunk.len());
        if next_len > expected {
            return Err(ClientError::ImageExceedsSize {
                image_id: self.metadata.image_id,
            });
        }
        self.data.extend_from_slice(chunk);
        Ok(())
    }

    fn into_parts(self) -> (ImageMetadata, Vec<u8>) {
        (self.metadata, self.data)
    }
}

#[derive(Debug, Clone)]
pub struct ImageAssembler {
    pending: HashMap<(u32, u32), PendingImage>,
}

impl Default for ImageAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageAssembler {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
        }
    }

    pub fn start(&mut self, request_id: u32, metadata: ImageMetadata) -> Result<(), ClientError> {
        debug!(
            "image start: request={request_id}, image_id={}, byte_len={}",
            metadata.image_id, metadata.byte_len
        );
        let key = (request_id, metadata.image_id);
        if self.pending.contains_key(&key) {
            warn!(
                "duplicate image {} for request {request_id}",
                metadata.image_id
            );
            return Err(ClientError::DuplicateImage {
                image_id: metadata.image_id,
                request_id,
            });
        }
        self.pending.insert(key, PendingImage::new(metadata)?);
        Ok(())
    }

    pub fn push_chunk(
        &mut self,
        request_id: u32,
        image_id: u32,
        data: &[u8],
    ) -> Result<(), ClientError> {
        debug!(
            "image chunk: request={request_id}, image_id={image_id}, len={}",
            data.len()
        );
        let pending = self
            .pending
            .get_mut(&(request_id, image_id))
            .ok_or_else(|| {
                warn!("unknown image {image_id} for request {request_id}");
                ClientError::UnknownImage {
                    image_id,
                    request_id,
                }
            })?;
        pending.push_chunk(data)
    }

    pub fn finish(
        &mut self,
        request_id: u32,
        image_id: u32,
    ) -> Result<(ImageMetadata, Vec<u8>), ClientError> {
        debug!("image finish: request={request_id}, image_id={image_id}");
        let pending = self
            .pending
            .remove(&(request_id, image_id))
            .ok_or_else(|| {
                warn!("unknown image {image_id} for request {request_id} at finish");
                ClientError::UnknownImage {
                    image_id,
                    request_id,
                }
            })?;
        let (metadata, data) = pending.into_parts();
        let actual_len = u64::try_from(data.len()).map_err(|_| ClientError::ImageTooLarge)?;
        if actual_len != metadata.byte_len {
            return Err(ClientError::ImageSizeMismatch {
                image_id,
                request_id,
                expected: metadata.byte_len,
                actual: actual_len,
            });
        }
        Ok((metadata, data))
    }

    pub fn drop_request(&mut self, request_id: u32) {
        debug!("dropping pending images for request {request_id}");
        self.pending
            .retain(|(pending_request_id, _), _| *pending_request_id != request_id);
    }
}
