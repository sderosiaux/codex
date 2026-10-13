//! Native launch inputs resolved from an `ExecParams` request.

use std::collections::HashMap;

use codex_exec_server_protocol::JSONRPCErrorError;
use codex_utils_absolute_path::AbsolutePathBuf;

use crate::protocol::ExecParams;
use crate::rpc::invalid_params;

pub(crate) struct PreparedExecRequest {
    pub(crate) command: Vec<String>,
    pub(crate) cwd: AbsolutePathBuf,
    pub(crate) env: HashMap<String, String>,
    pub(crate) arg0: Option<String>,
}

pub(crate) fn prepare_exec_request(
    params: &ExecParams,
    env: HashMap<String, String>,
) -> Result<PreparedExecRequest, JSONRPCErrorError> {
    let cwd = params.cwd.to_abs_path().map_err(|err| {
        invalid_params(format!(
            "cwd URI `{}` is not valid on this exec-server host: {err}",
            params.cwd
        ))
    })?;
    Ok(PreparedExecRequest {
        command: params.argv.clone(),
        cwd,
        env,
        arg0: params.arg0.clone(),
    })
}
