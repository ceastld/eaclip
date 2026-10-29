// Include the exact production retirement boundary without loading DirectX or
// touching a desktop. Failure injection covers recovery and ResizeBuffers.
#[path = "../../../vendor/gpui/src/platform/windows/directx_resource_lifecycle.rs"]
mod directx_resource_lifecycle;
