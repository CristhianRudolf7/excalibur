//! Handwritten command catalog for the pinned CDP subset; unknown response fields are tolerated.
use serde::{Deserialize, Serialize};
use std::time::Duration;
macro_rules! methods {
    ($($name:ident => $wire:literal),* $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Method { $($name),* }
        impl Method {
            pub const ALL: &'static [Self] = &[$(Self::$name),*];
            pub fn as_str(self) -> &'static str { match self { $(Self::$name => $wire),* } }
            pub fn timeout(self) -> Duration { Duration::from_secs(match self { Self::PageCaptureScreenshot => 15, Self::DOMSnapshotCaptureSnapshot => 20, _ => 10 }) }
        }
    };
}
methods! {
    BrowserGetVersion => "Browser.getVersion",
    BrowserClose => "Browser.close",
    BrowserSetDownloadBehavior => "Browser.setDownloadBehavior",
    BrowserCancelDownload => "Browser.cancelDownload",
    BrowserSetPermission => "Browser.setPermission",
    TargetCreateBrowserContext => "Target.createBrowserContext",
    TargetDisposeBrowserContext => "Target.disposeBrowserContext",
    TargetCreateTarget => "Target.createTarget",
    TargetCloseTarget => "Target.closeTarget",
    TargetAttachToTarget => "Target.attachToTarget",
    TargetDetachFromTarget => "Target.detachFromTarget",
    TargetSetAutoAttach => "Target.setAutoAttach",
    TargetSetDiscoverTargets => "Target.setDiscoverTargets",
    TargetGetTargetInfo => "Target.getTargetInfo",
    TargetGetTargets => "Target.getTargets",
    PageEnable => "Page.enable",
    PageNavigate => "Page.navigate",
    PageReload => "Page.reload",
    PageStopLoading => "Page.stopLoading",
    PageGetNavigationHistory => "Page.getNavigationHistory",
    PageNavigateToHistoryEntry => "Page.navigateToHistoryEntry",
    PageBringToFront => "Page.bringToFront",
    PageCaptureScreenshot => "Page.captureScreenshot",
    PageGetLayoutMetrics => "Page.getLayoutMetrics",
    PageGetFrameTree => "Page.getFrameTree",
    PageCreateIsolatedWorld => "Page.createIsolatedWorld",
    PageAddScriptToEvaluateOnNewDocument => "Page.addScriptToEvaluateOnNewDocument",
    PageSetLifecycleEventsEnabled => "Page.setLifecycleEventsEnabled",
    PageHandleJavaScriptDialog => "Page.handleJavaScriptDialog",
    PageSetInterceptFileChooserDialog => "Page.setInterceptFileChooserDialog",
    PageStartScreencast => "Page.startScreencast",
    PageStopScreencast => "Page.stopScreencast",
    PageScreencastFrameAck => "Page.screencastFrameAck",
    DOMGetFrameOwner => "DOM.getFrameOwner",
    AccessibilityGetPartialAXTree => "Accessibility.getPartialAXTree",
    DOMGetDocument => "DOM.getDocument",
    DOMQuerySelector => "DOM.querySelector",
    DOMEnable => "DOM.enable",
    DOMResolveNode => "DOM.resolveNode",
    DOMDescribeNode => "DOM.describeNode",
    DOMGetBoxModel => "DOM.getBoxModel",
    DOMGetContentQuads => "DOM.getContentQuads",
    DOMScrollIntoViewIfNeeded => "DOM.scrollIntoViewIfNeeded",
    DOMGetNodeForLocation => "DOM.getNodeForLocation",
    DOMFocus => "DOM.focus",
    DOMSetFileInputFiles => "DOM.setFileInputFiles",
    DOMGetOuterHTML => "DOM.getOuterHTML",
    DOMSnapshotCaptureSnapshot => "DOMSnapshot.captureSnapshot",
    AccessibilityEnable => "Accessibility.enable",
    AccessibilityGetFullAXTree => "Accessibility.getFullAXTree",
    InputDispatchMouseEvent => "Input.dispatchMouseEvent",
    InputDispatchKeyEvent => "Input.dispatchKeyEvent",
    InputInsertText => "Input.insertText",
    InputSetInterceptDrags => "Input.setInterceptDrags",
    InputDispatchDragEvent => "Input.dispatchDragEvent",
    RuntimeReleaseObject => "Runtime.releaseObject",
    NetworkSetUserAgentOverride => "Network.setUserAgentOverride",
    RuntimeEnable => "Runtime.enable",
    RuntimeCallFunctionOn => "Runtime.callFunctionOn",
    RuntimeAddBinding => "Runtime.addBinding",
    RuntimeRunIfWaitingForDebugger => "Runtime.runIfWaitingForDebugger",
    NetworkEnable => "Network.enable",
    NetworkDisable => "Network.disable",
    NetworkClearBrowserCache => "Network.clearBrowserCache",
    FetchEnable => "Fetch.enable",
    FetchContinueRequest => "Fetch.continueRequest",
    FetchFailRequest => "Fetch.failRequest",
    EmulationSetDeviceMetricsOverride => "Emulation.setDeviceMetricsOverride",
    EmulationClearDeviceMetricsOverride => "Emulation.clearDeviceMetricsOverride",
    EmulationSetFocusEmulationEnabled => "Emulation.setFocusEmulationEnabled",
    PerformanceEnable => "Performance.enable",
    PerformanceGetMetrics => "Performance.getMetrics",
    MemorySimulatePressureNotification => "Memory.simulatePressureNotification",
    HeapProfilerCollectGarbage => "HeapProfiler.collectGarbage",
    StorageGetUsageAndQuota => "Storage.getUsageAndQuota",
    StorageClearDataForOrigin => "Storage.clearDataForOrigin",
    TracingStart => "Tracing.start",
    TracingEnd => "Tracing.end",
    LogEnable => "Log.enable",
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Version {
    pub protocol_version: String,
    pub product: String,
    pub revision: String,
    pub user_agent: String,
    pub js_version: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetInfo {
    pub target_id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub attached: bool,
    pub browser_context_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attached {
    pub session_id: String,
    pub target_info: TargetInfo,
    #[serde(default)]
    pub waiting_for_debugger: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteObject {
    #[serde(rename = "type")]
    pub kind: String,
    pub object_id: Option<String>,
    pub value: Option<serde_json::Value>,
    pub description: Option<String>,
}

/// A typed command still shares the same correlation, timeout and framing path.
pub struct Command<P> {
    pub method: Method,
    pub params: P,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Empty {}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoAttach {
    pub auto_attach: bool,
    pub wait_for_debugger_on_start: bool,
    pub flatten: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachTarget {
    pub target_id: String,
    pub flatten: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTarget {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_context_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_window: Option<bool>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Navigate {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub referrer: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Screenshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_beyond_viewport: Option<bool>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureSnapshot {
    pub computed_styles: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_dom_rects: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_paint_order: Option<bool>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_node_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IsolatedWorld {
    pub frame_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub world_name: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallFunction {
    pub function_declaration: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_context_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub return_by_value: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub await_promise: Option<bool>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MouseEvent {
    #[serde(rename = "type")]
    pub kind: String,
    pub x: f64,
    pub y: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub button: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub click_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modifiers: Option<u32>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkBuffers {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_total_buffer_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_resource_buffer_size: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Frame {
    pub id: String,
    pub parent_id: Option<String>,
    pub loader_id: String,
    pub url: String,
    pub security_origin: Option<String>,
    pub mime_type: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameTree {
    pub frame: Frame,
    #[serde(default)]
    pub child_frames: Vec<FrameTree>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavigationResult {
    pub frame_id: String,
    pub loader_id: Option<String>,
    pub error_text: Option<String>,
    pub is_download: Option<bool>,
}

/// Event names in the supported subset. Unknown names remain usable by raw subscribers.
pub const EVENTS: &[&str] = &[
    "Browser.downloadWillBegin",
    "Browser.downloadProgress",
    "Target.targetCreated",
    "Target.targetInfoChanged",
    "Target.targetDestroyed",
    "Target.attachedToTarget",
    "Target.detachedFromTarget",
    "Page.lifecycleEvent",
    "Page.frameNavigated",
    "Page.frameRequestedNavigation",
    "Page.frameStartedNavigating",
    "Page.javascriptDialogOpening",
    "Page.fileChooserOpened",
    "Page.screencastFrame",
    "Input.dragIntercepted",
    "Runtime.consoleAPICalled",
    "Runtime.exceptionThrown",
    "Runtime.bindingCalled",
    "Network.requestWillBeSent",
    "Network.loadingFinished",
    "Network.loadingFailed",
    "Network.requestServedFromCache",
    "Network.responseReceived",
    "Fetch.requestPaused",
    "Tracing.dataCollected",
    "Tracing.tracingComplete",
    "Inspector.targetCrashed",
    "Inspector.detached",
    "Log.entryAdded",
];
