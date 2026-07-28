#![no_main]

use libfuzzer_sys::fuzz_target;
use serde::de::DeserializeOwned;
use serde::Serialize;
use stackhour_domain::{ClientCommand, HubToClient, HubToNode, NodeToHub};

fn round_trip<T>(data: &[u8])
where
    T: DeserializeOwned + Serialize,
{
    if let Ok(message) = serde_json::from_slice::<T>(data) {
        let encoded = serde_json::to_vec(&message).expect("a parsed protocol message must serialize");
        let _: T = serde_json::from_slice(&encoded).expect("a serialized protocol message must parse");
    }
}

fuzz_target!(|data: &[u8]| {
    round_trip::<ClientCommand>(data);
    round_trip::<HubToClient>(data);
    round_trip::<HubToNode>(data);
    round_trip::<NodeToHub>(data);
});
