use std::io::{Read, Write};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum CompressionError {
    #[error("Compression failed: {0}")]
    CompressFailed(String),
    #[error("Decompression failed: {0}")]
    DecompressFailed(String),
}

pub fn brotli_compress(data: &[u8], quality: u32) -> Result<Vec<u8>, CompressionError> {
    let mut output = Vec::new();
    let mut compressor = brotli::CompressorWriter::new(
        &mut output,
        4096,
        quality,
        22,
    );

    compressor
        .write_all(data)
        .map_err(|e| CompressionError::CompressFailed(e.to_string()))?;

    drop(compressor);
    Ok(output)
}

pub fn brotli_decompress(data: &[u8]) -> Result<Vec<u8>, CompressionError> {
    let mut output = Vec::new();
    let mut decompressor = brotli::Decompressor::new(data, 4096);

    decompressor
        .read_to_end(&mut output)
        .map_err(|e| CompressionError::DecompressFailed(e.to_string()))?;

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compress_decompress_roundtrip() {
        let original = b"Hello, world! This is a test of brotli compression.";
        let compressed = brotli_compress(original, 11).unwrap();
        let decompressed = brotli_decompress(&compressed).unwrap();
        assert_eq!(original.as_slice(), decompressed.as_slice());
    }

    #[test]
    fn test_compress_empty() {
        let original = b"";
        let compressed = brotli_compress(original, 11).unwrap();
        let decompressed = brotli_decompress(&compressed).unwrap();
        assert_eq!(original.as_slice(), decompressed.as_slice());
    }

    #[test]
    fn test_compress_html() {
        let html = b"<html><head><title>Test</title></head><body><h1>Hello</h1><p>This is a test page with some content.</p></body></html>";
        let compressed = brotli_compress(html, 11).unwrap();
        let decompressed = brotli_decompress(&compressed).unwrap();
        assert_eq!(html.as_slice(), decompressed.as_slice());

        let ratio = compressed.len() as f64 / html.len() as f64;
        assert!(ratio < 1.0, "Compression should reduce size");
    }

    #[test]
    fn test_decompress_invalid() {
        let invalid = b"not valid brotli data";
        assert!(brotli_decompress(invalid).is_err());
    }
}
