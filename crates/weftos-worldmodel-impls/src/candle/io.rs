//! Safetensors export / load for the candle models, and delivery through the RVF segment path.
//!
//! - **Files:** `save_safetensors` writes with candle's `VarMap::save`; `from_safetensors_path`
//!   reads with `candle_core::safetensors::load`.
//! - **Bytes (CAS / RVF):** [`to_safetensors_bytes`] writes the safetensors format in memory
//!   (candle 0.11 has no buffer writer; the format is an 8-byte LE header length, a JSON
//!   header, then the raw tensor data). `from_safetensors_bytes` reads with candle's
//!   `load_buffer`, so every blob we emit is checked by candle's own parser in the tests.
//! - **RVF:** `to_model_checkpoint` wraps the blob as a [`SmallModelKind::Encode`]
//!   [`ModelCheckpoint`]; on the wire it gets codec `0x03`
//!   ([`weftos_worldmodel_core::LEWM_MODEL_SEGMENT_CODEC_SAFETENSORS`]).
//!   [`active_candle_model`] builds the model from a [`SensorModelRegistry`]'s active Encode
//!   slot.
//!
//! Loading is strict: every tensor the architecture needs must be present with its exact
//! shape, and no unknown tensor may be present.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use candle_core::{DType, Device, Tensor};
use candle_nn::VarMap;
use weftos_worldmodel_core::{
    ModelCheckpoint, ModelHotSwap, SensorClass, SmallModelKind, TrainingSurfaceKind,
    LEWM_MODEL_SEGMENT_CODEC_SAFETENSORS,
};

use super::adaln::{AdaLnConfig, AdaNet};
use super::vit::{VitNet, VitTinyConfig};
use super::{
    invalid, read_config, AdaLnPredictor, CandleModelError, CandleResult, CandleVitEncoder,
    ARCH_ADALN, ARCH_VIT_TINY, CONFIG_TENSOR,
};
use crate::training::{codec_for_weights, SensorModelRegistry};

/// Serialise `F32` tensors as a safetensors blob, keys in sorted order (deterministic bytes,
/// stable digest). Names must be ASCII `[A-Za-z0-9._-]`.
pub fn to_safetensors_bytes(tensors: &BTreeMap<String, Tensor>) -> CandleResult<Vec<u8>> {
    let mut header = String::from("{");
    let mut data: Vec<u8> = Vec::new();
    for (i, (name, t)) in tensors.iter().enumerate() {
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) {
            return invalid(format!("tensor name {name:?} is not [A-Za-z0-9._-]+"));
        }
        let vals: Vec<f32> = t.to_dtype(DType::F32)?.flatten_all()?.to_vec1()?;
        let start = data.len();
        for v in &vals {
            data.extend_from_slice(&v.to_le_bytes());
        }
        let shape: Vec<String> = t.dims().iter().map(|d| d.to_string()).collect();
        if i > 0 {
            header.push(',');
        }
        header.push_str(&format!(
            "\"{name}\":{{\"dtype\":\"F32\",\"shape\":[{}],\"data_offsets\":[{start},{}]}}",
            shape.join(","),
            data.len()
        ));
    }
    header.push('}');
    while (8 + header.len()) % 8 != 0 {
        header.push(' ');
    }
    let mut out = Vec::with_capacity(8 + header.len() + data.len());
    out.extend_from_slice(&(header.len() as u64).to_le_bytes());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(&data);
    Ok(out)
}

fn varmap_tensors(varmap: &VarMap) -> CandleResult<BTreeMap<String, Tensor>> {
    let data = varmap.data().lock().map_err(|_| CandleModelError::Invalid("varmap lock poisoned".into()))?;
    Ok(data.iter().map(|(k, v)| (k.clone(), v.as_tensor().clone())).collect())
}

/// Copy `loaded` into `varmap`: same names, same shapes, nothing missing, nothing extra.
fn load_into_varmap(varmap: &VarMap, mut loaded: HashMap<String, Tensor>) -> CandleResult<()> {
    let data = varmap.data().lock().map_err(|_| CandleModelError::Invalid("varmap lock poisoned".into()))?;
    let mut names: Vec<&String> = data.keys().collect();
    names.sort();
    for name in names {
        let var = &data[name];
        let t = loaded.remove(name).ok_or_else(|| CandleModelError::Invalid(format!("checkpoint is missing tensor {name}")))?;
        if t.dims() != var.dims() {
            return invalid(format!("tensor {name}: checkpoint shape {:?}, model expects {:?}", t.dims(), var.dims()));
        }
        var.set(&t.to_dtype(DType::F32)?)?;
    }
    if let Some(extra) = loaded.keys().min() {
        return invalid(format!("checkpoint has unknown tensor {extra} ({} extra in all)", loaded.len()));
    }
    Ok(())
}

fn config_of(loaded: &HashMap<String, Tensor>, arch: f32) -> CandleResult<[usize; 6]> {
    let t = loaded.get(CONFIG_TENSOR).ok_or_else(|| CandleModelError::Invalid(format!("checkpoint has no {CONFIG_TENSOR} tensor")))?;
    read_config(t, arch)
}

fn read_path(path: &Path) -> CandleResult<HashMap<String, Tensor>> {
    candle_core::safetensors::load(path, &Device::Cpu)
        .map_err(|e| CandleModelError::Invalid(format!("reading {}: {e}", path.display())))
}

fn read_bytes(bytes: &[u8]) -> CandleResult<HashMap<String, Tensor>> {
    candle_core::safetensors::load_buffer(bytes, &Device::Cpu)
        .map_err(|e| CandleModelError::Invalid(format!("not a safetensors blob: {e}")))
}

fn checkpoint(blob: Vec<u8>, class: SensorClass, version: u64, surface: TrainingSurfaceKind, tick: Option<u64>) -> ModelCheckpoint {
    debug_assert_eq!(codec_for_weights(&blob), LEWM_MODEL_SEGMENT_CODEC_SAFETENSORS);
    ModelCheckpoint::new(class, SmallModelKind::Encode, version, surface, blob, tick)
}

impl CandleVitEncoder {
    fn from_tensors(loaded: HashMap<String, Tensor>) -> CandleResult<Self> {
        let cfg = VitTinyConfig::from_params(config_of(&loaded, ARCH_VIT_TINY)?)?;
        let net = VitNet::build(&cfg, 0)?;
        load_into_varmap(&net.varmap, loaded)?;
        Ok(Self { config: cfg, weights_path: None, net: Some(net) })
    }

    /// Load a checkpoint file.
    pub fn from_safetensors_path(path: impl AsRef<Path>) -> CandleResult<Self> {
        let mut enc = Self::from_tensors(read_path(path.as_ref())?)?;
        enc.weights_path = Some(path.as_ref().display().to_string());
        Ok(enc)
    }

    /// Load a checkpoint blob (CAS object, RVF segment payload).
    pub fn from_safetensors_bytes(bytes: &[u8]) -> CandleResult<Self> {
        Self::from_tensors(read_bytes(bytes)?)
    }

    fn net(&self) -> CandleResult<&VitNet> {
        self.net.as_ref().ok_or_else(|| CandleModelError::Invalid("vit encoder has no weights".into()))
    }

    /// Export as a safetensors blob.
    pub fn to_safetensors_bytes(&self) -> CandleResult<Vec<u8>> {
        to_safetensors_bytes(&varmap_tensors(&self.net()?.varmap)?)
    }

    /// Write a safetensors file (candle `VarMap::save`).
    pub fn save_safetensors(&self, path: impl AsRef<Path>) -> CandleResult<()> {
        Ok(self.net()?.varmap.save(path)?)
    }

    /// Wrap the weights as an Encode-slot checkpoint (codec `0x03` on the wire).
    pub fn to_model_checkpoint(&self, class: SensorClass, version: u64, surface: TrainingSurfaceKind, target_tick: Option<u64>) -> CandleResult<ModelCheckpoint> {
        Ok(checkpoint(self.to_safetensors_bytes()?, class, version, surface, target_tick))
    }
}

impl AdaLnPredictor {
    fn from_tensors(loaded: HashMap<String, Tensor>) -> CandleResult<Self> {
        let cfg = AdaLnConfig::from_params(config_of(&loaded, ARCH_ADALN)?)?;
        let net = AdaNet::build(&cfg, 0)?;
        load_into_varmap(&net.varmap, loaded)?;
        Ok(Self { weights_path: None, hidden_dim: cfg.hidden_dim, config: cfg, net: Some(net) })
    }

    /// Load a checkpoint file.
    pub fn from_safetensors_path(path: impl AsRef<Path>) -> CandleResult<Self> {
        let mut p = Self::from_tensors(read_path(path.as_ref())?)?;
        p.weights_path = Some(path.as_ref().display().to_string());
        Ok(p)
    }

    /// Load a checkpoint blob (CAS object, RVF segment payload).
    pub fn from_safetensors_bytes(bytes: &[u8]) -> CandleResult<Self> {
        Self::from_tensors(read_bytes(bytes)?)
    }

    fn net(&self) -> CandleResult<&AdaNet> {
        self.net.as_ref().ok_or_else(|| CandleModelError::Invalid("adaln predictor has no weights".into()))
    }

    /// Export as a safetensors blob.
    pub fn to_safetensors_bytes(&self) -> CandleResult<Vec<u8>> {
        to_safetensors_bytes(&varmap_tensors(&self.net()?.varmap)?)
    }

    /// Write a safetensors file (candle `VarMap::save`).
    pub fn save_safetensors(&self, path: impl AsRef<Path>) -> CandleResult<()> {
        Ok(self.net()?.varmap.save(path)?)
    }

    /// Wrap the weights as an Encode-slot checkpoint (codec `0x03` on the wire).
    pub fn to_model_checkpoint(&self, class: SensorClass, version: u64, surface: TrainingSurfaceKind, target_tick: Option<u64>) -> CandleResult<ModelCheckpoint> {
        Ok(checkpoint(self.to_safetensors_bytes()?, class, version, surface, target_tick))
    }
}

/// A candle model rebuilt from a safetensors checkpoint.
#[derive(Debug, Clone)]
pub enum CandleModel {
    /// ViT-tiny encoder.
    Encoder(CandleVitEncoder),
    /// AdaLN `pred_φ`.
    Predictor(AdaLnPredictor),
}

impl CandleModel {
    /// Dispatch on the checkpoint's `lewm.config` architecture tag.
    pub fn from_safetensors_bytes(bytes: &[u8]) -> CandleResult<Self> {
        let loaded = read_bytes(bytes)?;
        let arch = loaded
            .get(CONFIG_TENSOR)
            .ok_or_else(|| CandleModelError::Invalid(format!("checkpoint has no {CONFIG_TENSOR} tensor")))?
            .flatten_all()?
            .get(0)?
            .to_scalar::<f32>()?;
        if arch == ARCH_VIT_TINY {
            Ok(Self::Encoder(CandleVitEncoder::from_tensors(loaded)?))
        } else if arch == ARCH_ADALN {
            Ok(Self::Predictor(AdaLnPredictor::from_tensors(loaded)?))
        } else {
            invalid(format!("checkpoint architecture {arch} is neither vit_tiny (1) nor adaln (2)"))
        }
    }

    /// The encoder, if this is one.
    pub fn into_encoder(self) -> Option<CandleVitEncoder> {
        match self {
            Self::Encoder(e) => Some(e),
            Self::Predictor(_) => None,
        }
    }

    /// The predictor, if this is one.
    pub fn into_predictor(self) -> Option<AdaLnPredictor> {
        match self {
            Self::Predictor(p) => Some(p),
            Self::Encoder(_) => None,
        }
    }
}

/// Build the candle model held by `class`'s active Encode slot.
///
/// `Ok(None)` when the slot is empty or holds a non-safetensors payload (codec `0x02`, the
/// action-linear blob); `Err` when it holds a safetensors blob that does not load.
pub fn active_candle_model(registry: &SensorModelRegistry, class: SensorClass) -> CandleResult<Option<CandleModel>> {
    let Some(cp) = registry.active(class, SmallModelKind::Encode) else {
        return Ok(None);
    };
    if codec_for_weights(&cp.weights) != LEWM_MODEL_SEGMENT_CODEC_SAFETENSORS {
        return Ok(None);
    }
    CandleModel::from_safetensors_bytes(&cp.weights).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candle::{adaln, vit};
    use crate::training::{model_segment_from_wire, model_segment_to_wire, encode_model_segment};
    use weftos_worldmodel_core::{zero_latent, Action, Encoder, Predictor};

    #[test]
    fn our_bytes_parse_with_candle_and_match_candle_saved_files() {
        let enc = CandleVitEncoder::new_random_with(vit::tests::small(), 9).unwrap();
        let bytes = enc.to_safetensors_bytes().unwrap();
        assert_eq!(bytes, enc.to_safetensors_bytes().unwrap(), "deterministic");
        let from_bytes = CandleVitEncoder::from_safetensors_bytes(&bytes).unwrap();

        let dir = std::env::temp_dir().join(format!("lewm-io-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vit.safetensors");
        enc.save_safetensors(&path).unwrap();
        let from_file = CandleVitEncoder::from_safetensors_path(&path).unwrap();
        let from_file_bytes = CandleVitEncoder::from_safetensors_bytes(&std::fs::read(&path).unwrap()).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        let f = vit::tests::frame(&enc.config, 5);
        let z = enc.encode(&f).unwrap();
        assert_eq!(from_bytes.encode(&f).unwrap(), z);
        assert_eq!(from_file.encode(&f).unwrap(), z);
        assert_eq!(from_file_bytes.encode(&f).unwrap(), z);
        assert!(from_file.weights_path.as_deref().unwrap().ends_with("vit.safetensors"));
    }

    #[test]
    fn loading_is_strict_about_architecture_and_tensors() {
        let pred = AdaLnPredictor::new_random_with(adaln::tests::small(), 2).unwrap();
        let blob = pred.to_safetensors_bytes().unwrap();
        let err = CandleVitEncoder::from_safetensors_bytes(&blob).unwrap_err().to_string();
        assert!(err.contains("expected a vit_tiny encoder"), "{err}");

        let mut tensors = varmap_tensors(&pred.net.as_ref().unwrap().varmap).unwrap();
        tensors.remove("final.out.weight");
        let err = AdaLnPredictor::from_safetensors_bytes(&to_safetensors_bytes(&tensors).unwrap()).unwrap_err().to_string();
        assert!(err.contains("missing tensor final.out.weight"), "{err}");

        let mut tensors = varmap_tensors(&pred.net.as_ref().unwrap().varmap).unwrap();
        tensors.insert("stray".into(), Tensor::zeros(2, DType::F32, &Device::Cpu).unwrap());
        let err = AdaLnPredictor::from_safetensors_bytes(&to_safetensors_bytes(&tensors).unwrap()).unwrap_err().to_string();
        assert!(err.contains("unknown tensor stray"), "{err}");

        assert!(AdaLnPredictor::from_safetensors_bytes(b"not safetensors").is_err());
        assert!(to_safetensors_bytes(&BTreeMap::from([("bad\"name".to_string(), Tensor::zeros(1, DType::F32, &Device::Cpu).unwrap())])).is_err());
    }

    #[test]
    fn rvf_delivery_to_the_registry_builds_both_models() {
        let enc = CandleVitEncoder::new_random_with(vit::tests::small(), 4).unwrap();
        let pred = AdaLnPredictor::new_random_with(adaln::tests::small(), 4).unwrap();
        let mut reg = SensorModelRegistry::new();
        assert!(active_candle_model(&reg, SensorClass::RgbD).unwrap().is_none());

        // Encoder: checkpoint → wire (codec 0x03) → stage_wire → commit → helper.
        let cp = enc.to_model_checkpoint(SensorClass::RgbD, 1, TrainingSurfaceKind::OfflineEdge, Some(2)).unwrap();
        let wire = model_segment_to_wire(&encode_model_segment(&cp));
        assert_eq!(wire[1], LEWM_MODEL_SEGMENT_CODEC_SAFETENSORS);
        assert_eq!(model_segment_from_wire(&wire).unwrap().codec, 0x03);
        reg.stage_wire(&wire, TrainingSurfaceKind::OfflineEdge, Some(2)).unwrap();
        reg.commit_at_tick(2).unwrap();
        let got = active_candle_model(&reg, SensorClass::RgbD).unwrap().unwrap().into_encoder().unwrap();
        let f = vit::tests::frame(&enc.config, 1);
        assert_eq!(got.encode(&f).unwrap(), enc.encode(&f).unwrap());

        // Predictor in another class's Encode slot.
        let cp = pred.to_model_checkpoint(SensorClass::Imu, 1, TrainingSurfaceKind::OfflineEdge, Some(3)).unwrap();
        reg.deliver_offline_checkpoint(cp, 3).unwrap();
        let got = active_candle_model(&reg, SensorClass::Imu).unwrap().unwrap().into_predictor().unwrap();
        assert_eq!(got.predict(&zero_latent(), &Action::null()).unwrap(), zero_latent());

        // A raw action-linear checkpoint in the slot is not a candle model.
        let raw = crate::training::ActionLinearModel::zero().to_weights();
        let cp = ModelCheckpoint::new(SensorClass::Audio, SmallModelKind::Encode, 1, TrainingSurfaceKind::OfflineEdge, raw, Some(4));
        reg.deliver_offline_checkpoint(cp, 4).unwrap();
        assert!(active_candle_model(&reg, SensorClass::Audio).unwrap().is_none());
    }
}
