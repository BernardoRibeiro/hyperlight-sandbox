# hyperlight-sandbox-backend-wasm

Native Wasm backend package for the `hyperlight-sandbox` Python API.

The backend accepts the stable API's Phase 2 mount options:

- `work_dir`: existing host directory exposed as `/work`;
- `work_dir_access`: `"ro"` by default or explicit `"rw"`;
- `temp_dir`: create a private, recursively cleaned `/tmp` mount.

The backend validates the work directory and access mode before lazily
constructing the guest. Host paths are used only to create preopen
capabilities; guest code sees `/work` and `/tmp`, not ambient host paths.
