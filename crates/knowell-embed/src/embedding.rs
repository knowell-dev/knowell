//! Vectors and embedding inputs.

use crate::error::EmbedError;

/// A document chunk to embed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentInput {
    /// Optional title, typically the file path or symbol name. Providers
    /// that support titles (Gemini Embedding 2) place it in the prompt.
    pub title: Option<String>,
    /// The chunk text. Must not be empty.
    pub text: String,
}

impl DocumentInput {
    /// A document without a title.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            title: None,
            text: text.into(),
        }
    }

    /// A document with a title.
    pub fn with_title(title: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            title: Some(title.into()),
            text: text.into(),
        }
    }
}

/// An L2-normalised embedding vector (unit length, all components finite).
///
/// The invariant is enforced at construction so that cosine similarity is a
/// plain dot product everywhere downstream.
#[derive(Debug, Clone, PartialEq)]
pub struct Embedding(Vec<f32>);

impl Embedding {
    /// Normalises `values` to unit length.
    ///
    /// Fails for an empty vector, a non-finite component, or a zero vector,
    /// none of which is a valid embedding.
    pub fn from_values(values: Vec<f32>) -> Result<Self, EmbedError> {
        if values.is_empty() {
            return Err(EmbedError::Vector("embedding has no components".into()));
        }
        if values.iter().any(|v| !v.is_finite()) {
            return Err(EmbedError::Vector(
                "embedding contains a non-finite component".into(),
            ));
        }
        // f64 accumulation keeps the norm stable for 3072-dimensional inputs.
        let norm = values
            .iter()
            .map(|v| f64::from(*v) * f64::from(*v))
            .sum::<f64>()
            .sqrt();
        if norm < 1e-12 {
            return Err(EmbedError::Vector("embedding is the zero vector".into()));
        }
        let normalised = values
            .into_iter()
            .map(|v| (f64::from(v) / norm) as f32)
            .collect();
        Ok(Self(normalised))
    }

    /// The components.
    pub fn as_slice(&self) -> &[f32] {
        &self.0
    }

    /// Consumes the embedding, returning the components.
    pub fn into_vec(self) -> Vec<f32> {
        self.0
    }

    /// Number of components.
    pub fn dimensions(&self) -> usize {
        self.0.len()
    }

    /// Cosine similarity with `other` (a dot product, as both are unit
    /// length). `None` when the dimensions differ: vectors of different
    /// profiles must never be compared.
    pub fn cosine(&self, other: &Self) -> Option<f32> {
        if self.0.len() != other.0.len() {
            return None;
        }
        let dot: f64 = self
            .0
            .iter()
            .zip(&other.0)
            .map(|(a, b)| f64::from(*a) * f64::from(*b))
            .sum();
        Some(dot as f32)
    }
}

/// Keeps the first `new_dims` components of `embedding` and re-normalises.
///
/// This is the Matryoshka-style downgrade between profiles that differ only
/// in dimensionality, done without any API call. It is valid **only** for
/// models trained with Matryoshka representation learning, where the leading
/// components form a usable lower-dimensional embedding (Gemini Embedding 2 is;
/// arbitrary models are not). The caller must give the result a new profile.
///
/// Fails when `new_dims` is zero or larger than the vector: a vector is
/// never grown.
pub fn truncate_and_normalize(
    embedding: &Embedding,
    new_dims: usize,
) -> Result<Embedding, EmbedError> {
    if new_dims == 0 {
        return Err(EmbedError::Vector(
            "cannot truncate an embedding to zero dimensions".into(),
        ));
    }
    if new_dims > embedding.dimensions() {
        return Err(EmbedError::Vector(format!(
            "cannot grow an embedding from {} to {new_dims} dimensions",
            embedding.dimensions()
        )));
    }
    Embedding::from_values(embedding.0.iter().copied().take(new_dims).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_to_unit_length() {
        let e = Embedding::from_values(vec![3.0, 4.0]).unwrap();
        assert!((e.as_slice()[0] - 0.6).abs() < 1e-6);
        assert!((e.as_slice()[1] - 0.8).abs() < 1e-6);
        assert!((e.cosine(&e).unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn rejects_degenerate_vectors() {
        assert!(Embedding::from_values(vec![]).is_err());
        assert!(Embedding::from_values(vec![0.0, 0.0]).is_err());
        assert!(Embedding::from_values(vec![1.0, f32::NAN]).is_err());
        assert!(Embedding::from_values(vec![f32::INFINITY]).is_err());
    }

    #[test]
    fn cosine_refuses_different_dimensions() {
        let a = Embedding::from_values(vec![1.0, 0.0]).unwrap();
        let b = Embedding::from_values(vec![1.0, 0.0, 0.0]).unwrap();
        assert_eq!(a.cosine(&b), None);
    }

    #[test]
    fn truncation_renormalises_and_never_grows() {
        let e = Embedding::from_values(vec![1.0, 1.0, 1.0, 1.0]).unwrap();
        let t = truncate_and_normalize(&e, 2).unwrap();
        assert_eq!(t.dimensions(), 2);
        let norm: f32 = t.as_slice().iter().map(|v| v * v).sum();
        assert!((norm - 1.0).abs() < 1e-6);
        assert!(truncate_and_normalize(&e, 0).is_err());
        assert!(truncate_and_normalize(&e, 5).is_err());
        assert_eq!(truncate_and_normalize(&e, 4).unwrap(), e);
    }

    #[test]
    fn truncating_a_vector_whose_prefix_is_zero_fails() {
        let e = Embedding::from_values(vec![0.0, 0.0, 1.0]).unwrap();
        assert!(truncate_and_normalize(&e, 2).is_err());
    }
}
