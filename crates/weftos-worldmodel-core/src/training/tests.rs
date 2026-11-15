//! Cross-module tests for the training contracts.

use super::*;
use crate::latent::zero_latent;

    #[test]
    fn small_model_segment_bytes_stable() {
        assert_eq!(SmallModelKind::TransmitGate as u8, 0x60);
        assert_eq!(SmallModelKind::Aggregate as u8, 0x61);
        assert_eq!(SmallModelKind::Encode as u8, 0x62);
        for k in SmallModelKind::ALL {
            assert_eq!(SmallModelKind::try_from(k as u8), Ok(k));
            assert!(k.segment_type() >= LEWM_MODEL_SEGMENT_DOMAIN_START);
            assert!(k.segment_type() <= LEWM_MODEL_SEGMENT_DOMAIN_END);
        }
    }

    #[test]
    fn sample_residual_and_digest() {
        let mut z0 = zero_latent();
        let mut z1 = zero_latent();
        z0[0] = 1.0;
        z1[0] = 3.0;
        let s = TrainingSample::basic(SensorClass::Imu, z0, z1, 10);
        let r = s.residual();
        assert!((r[0] - 2.0).abs() < 1e-6);

        let mut cp = ModelCheckpoint::new(
            SensorClass::Imu,
            SmallModelKind::Encode,
            1,
            TrainingSurfaceKind::OfflineEdge,
            alloc::vec![1, 2, 3],
            Some(5),
        );
        let d = cp.weight_digest;
        cp.recompute_digest();
        assert_eq!(cp.weight_digest, d);
        assert_eq!(fnv1a64(&[1, 2, 3]), d);
    }

    #[test]
    fn surface_names() {
        assert_eq!(
            TrainingSurfaceKind::OfflineEdge.as_str(),
            "offline_edge"
        );
        assert_eq!(
            TrainingSurfaceKind::OnlineStreamingMerge.as_str(),
            "online_streaming_merge"
        );
        assert_eq!(EVENT_KIND_TRAINING_CYCLE, "lewm.training_cycle");
        assert_eq!(EVENT_KIND_MODEL_HOT_SWAP, "lewm.model_hot_swap");
    }
