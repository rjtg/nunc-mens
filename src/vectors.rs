use anyhow::{Result, ensure};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Model {
    pub id: &'static str,
    pub revision: &'static str,
    pub dimensions: usize,
}

pub(crate) trait Embedder {
    fn model(&self) -> Model;
    fn passages(&mut self, passages: &[&str]) -> Result<Vec<Vec<f32>>>;
}

pub(crate) fn norm(vector: &[f32], dimensions: usize) -> Result<f64> {
    ensure!(
        vector.len() == dimensions,
        "invalid embedding dimensions: expected {dimensions}, got {}",
        vector.len()
    );
    let squared: f64 = vector.iter().map(|&value| f64::from(value).powi(2)).sum();
    ensure!(
        squared.is_finite() && squared > 0.0,
        "embedding must be finite and nonzero"
    );
    Ok(squared.sqrt())
}

pub(crate) fn encode(vector: &[f32], dimensions: usize) -> Result<Vec<u8>> {
    norm(vector, dimensions)?;
    Ok(vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect())
}

pub(crate) fn decode(bytes: &[u8], dimensions: usize) -> Result<Vec<f32>> {
    ensure!(
        dimensions.checked_mul(4) == Some(bytes.len()),
        "invalid embedding BLOB length"
    );
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .copied()
        .map(f32::from_le_bytes)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vectors_validate_and_roundtrip() -> Result<()> {
        let bytes = encode(&[1.0, -2.0], 2)?;
        assert_eq!(bytes, [0, 0, 128, 63, 0, 0, 0, 192]);
        assert_eq!(decode(&bytes, 2)?, [1.0, -2.0]);
        assert!(decode(&bytes, 3).is_err());
        assert!(encode(&[f32::NAN], 1).is_err());
        assert!(encode(&[0.0], 1).is_err());
        assert!(encode(&[1.0], 2).is_err());
        Ok(())
    }
}
