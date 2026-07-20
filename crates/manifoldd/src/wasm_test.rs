use manifold_wasm::TestHarnessRequest;

use crate::{control, state::Manifold};

pub fn service_request(state: &mut Manifold, request: TestHarnessRequest) {
    match request {
        TestHarnessRequest::CommandJson { input, response } => {
            let result =
                control::execute_json_command(&input, state).map_err(|err| err.to_string());
            if response.send(result).is_err() {
                tracing::debug!("test harness dropped its command response receiver");
            }
        }
        TestHarnessRequest::StatusJson { response } => {
            let result = control::status_json_string(state).map_err(|err| err.to_string());
            if response.send(result).is_err() {
                tracing::debug!("test harness dropped its status response receiver");
            }
        }
    }
}
