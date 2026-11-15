//! Facade and typed-dispatch tests.

use super::*;
use serde_json::{json, Value};
use weftos_worldmodel_core::{LatticeMethod, SubscriptionId, WorldModelError, LATENT_DIM};
use weftos_worldmodel_impls::StubLattice;


fn facade() -> LatticeServiceFacade<StubLattice> {
    default_lattice_service()
}

fn zero_json() -> Value {
    json!(vec![0.0f32; LATENT_DIM])
}

#[test]
fn seven_methods_registered() {
    assert_eq!(LATTICE_METHODS.len(), 7);
    assert_eq!(LatticeServiceFacade::<StubLattice>::method_names().len(), 7);
    for m in LATTICE_METHODS {
        assert!(parse_lattice_method(m.as_str()).is_some());
    }
}

#[test]
fn list_and_health() {
    let f = facade();
    let services = f.list_services();
    assert_eq!(services.len(), 1);
    assert_eq!(services[0].name, LATTICE_SERVICE_NAME);
    assert!(services[0].healthy);
    assert_eq!(
        f.health(LATTICE_SERVICE_NAME).unwrap(),
        FacadeHealth::Healthy
    );
    assert!(matches!(
        f.health("nope"),
        Err(LatticeServiceError::ServiceNotFound(_))
    ));
}

#[test]
fn observe_happy_and_dim_error() {
    let f = facade();
    let ok = f
        .call(
            LATTICE_SERVICE_NAME,
            "observe",
            json!({"bytes": "frame", "timestamp_ms": 1}),
        )
        .unwrap();
    assert_eq!(ok["latent_dim"], LATENT_DIM);

    let err = f
        .call(
            LATTICE_SERVICE_NAME,
            "observe",
            json!({"bytes": "x", "latent_dim": 64}),
        )
        .unwrap_err();
    assert!(matches!(
        err,
        LatticeServiceError::WorldModel(WorldModelError::LatentDimMismatch { got: 64, .. })
    ));
}

#[test]
fn observe_node_happy_and_service_error() {
    let f = facade();
    let ok = f
        .call(
            LATTICE_SERVICE_NAME,
            "observe_node",
            json!({"node": 9, "bytes": "n", "timestamp_ms": 2}),
        )
        .unwrap();
    assert_eq!(ok["node"], 9);

    assert!(matches!(
        f.call("other", "observe_node", json!({})),
        Err(LatticeServiceError::ServiceNotFound(_))
    ));
}

#[test]
fn predict_happy_and_bad_latent() {
    let f = facade();
    let ok = f
        .call(
            LATTICE_SERVICE_NAME,
            "predict",
            json!({"z_t": zero_json(), "action": vec![0.0f32; 32]}),
        )
        .unwrap();
    assert_eq!(ok["latent_dim"], LATENT_DIM);

    let err = f
        .call(
            LATTICE_SERVICE_NAME,
            "predict",
            json!({"z_t": vec![0.0f32; 8]}),
        )
        .unwrap_err();
    assert!(matches!(
        err,
        LatticeServiceError::WorldModel(WorldModelError::LatentDimMismatch { .. })
    ));
}

#[test]
fn plan_happy_and_missing_latent() {
    let f = facade();
    let ok = f
        .call(
            LATTICE_SERVICE_NAME,
            "plan",
            json!({"z_t": zero_json(), "horizon": 5}),
        )
        .unwrap();
    assert_eq!(ok["horizon"], 5);

    let err = f
        .call(LATTICE_SERVICE_NAME, "plan", json!({"horizon": 2}))
        .unwrap_err();
    assert!(matches!(err, LatticeServiceError::BadParams(_)));
}

#[test]
fn recall_happy_and_method_not_found() {
    let f = facade();
    let ok = f
        .call(
            LATTICE_SERVICE_NAME,
            "recall",
            json!({"query": zero_json(), "k": 3}),
        )
        .unwrap();
    assert!(ok["hits"].as_array().unwrap().is_empty());

    assert!(matches!(
        f.call(LATTICE_SERVICE_NAME, "nope", json!({})),
        Err(LatticeServiceError::MethodNotFound(_))
    ));
}

#[test]
fn subscribe_surprise_and_drift() {
    let f = facade();
    let s = f
        .call(
            LATTICE_SERVICE_NAME,
            "subscribe_surprise",
            json!({"handle": 11}),
        )
        .unwrap();
    assert_eq!(s["handle"], 11);
    let d = f
        .call(
            LATTICE_SERVICE_NAME,
            "subscribe_drift",
            json!({"handle": 12}),
        )
        .unwrap();
    assert_eq!(d["subscribed"], "drift");

    f.with_lattice(|lat| {
        assert!(lat.surprise_subs.contains(&SubscriptionId(11)));
        assert!(lat.drift_subs.contains(&SubscriptionId(12)));
    });
}

#[test]
fn offline_returns_unavailable() {
    let mut f = facade();
    f.set_online(false);
    assert_eq!(f.health(LATTICE_SERVICE_NAME).unwrap(), FacadeHealth::Unhealthy);
    assert!(matches!(
        f.call(LATTICE_SERVICE_NAME, "observe", json!({})),
        Err(LatticeServiceError::WorldModel(WorldModelError::Unavailable))
    ));
}

#[test]
fn observe_json_helper() {
    let f = facade();
    let v = observe_json(&f, b"hello", 42).unwrap();
    assert_eq!(v["latent"].as_array().unwrap().len(), LATENT_DIM);
}

#[test]
fn dispatch_typed_all_methods() {
    let mut lat = StubLattice::default();
    for m in LATTICE_METHODS {
        let params = match m {
            LatticeMethod::Observe | LatticeMethod::ObserveNode => {
                json!({"bytes": "x", "node": 1, "timestamp_ms": 0})
            }
            LatticeMethod::Predict => json!({"z_t": zero_json(), "action": vec![0.0f32; 32]}),
            LatticeMethod::Plan => json!({"z_t": zero_json(), "horizon": 2}),
            LatticeMethod::Recall => json!({"query": zero_json(), "k": 1}),
            LatticeMethod::SubscribeSurprise | LatticeMethod::SubscribeDrift => {
                json!({"handle": 0})
            }
            _ => json!({}),
        };
        dispatch_typed(&mut lat, *m, &params).unwrap();
    }
}
