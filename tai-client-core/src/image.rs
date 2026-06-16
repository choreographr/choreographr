use std::{collections::HashMap, io};
use tai_proto::ImageMetadata;

#[derive(Debug, Clone)]
pub struct PendingImage {
    metadata: ImageMetadata,
    data: Vec<u8>,
}

impl PendingImage {
    fn new(metadata: ImageMetadata) -> io::Result<Self> {
        let capacity = usize::try_from(metadata.byte_len).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "image byte length does not fit in memory",
            )
        })?;
        Ok(Self {
            metadata,
            data: Vec::with_capacity(capacity),
        })
    }

    fn push_chunk(&mut self, chunk: &[u8]) -> io::Result<()> {
        let expected = usize::try_from(self.metadata.byte_len).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "image byte length does not fit in memory",
            )
        })?;
        let next_len = self.data.len().saturating_add(chunk.len());
        if next_len > expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("image {} exceeded advertised size", self.metadata.image_id),
            ));
        }
        self.data.extend_from_slice(chunk);
        Ok(())
    }

    fn into_parts(self) -> (ImageMetadata, Vec<u8>) {
        (self.metadata, self.data)
    }
}

#[derive(Debug)]
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

    pub fn start(&mut self, request_id: u32, metadata: ImageMetadata) -> io::Result<()> {
        let key = (request_id, metadata.image_id);
        if self.pending.contains_key(&key) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "image {} for request {} already exists",
                    metadata.image_id, request_id
                ),
            ));
        }
        self.pending.insert(key, PendingImage::new(metadata)?);
        Ok(())
    }

    pub fn push_chunk(&mut self, request_id: u32, image_id: u32, data: &[u8]) -> io::Result<()> {
        let pending = self
            .pending
            .get_mut(&(request_id, image_id))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "received image chunk for unknown image {image_id} request {request_id}"
                    ),
                )
            })?;
        pending.push_chunk(data)
    }

    pub fn finish(
        &mut self,
        request_id: u32,
        image_id: u32,
    ) -> io::Result<(ImageMetadata, Vec<u8>)> {
        let pending = self
            .pending
            .remove(&(request_id, image_id))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("received image end for unknown image {image_id} request {request_id}"),
                )
            })?;
        let (metadata, data) = pending.into_parts();
        let actual_len = u64::try_from(data.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "image size does not fit in u64")
        })?;
        if actual_len != metadata.byte_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "image {} for request {} ended with {} bytes but expected {}",
                    image_id, request_id, actual_len, metadata.byte_len
                ),
            ));
        }
        Ok((metadata, data))
    }

    pub fn drop_request(&mut self, request_id: u32) {
        self.pending
            .retain(|(pending_request_id, _), _| *pending_request_id != request_id);
    }
}
