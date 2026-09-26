//! Keep EGL extension slots for Skia (Mesa-based Android, e.g. Waydroid).
//!
//! Android's libEGL gives each GL function it has no built-in entry point for
//! one of a fixed number of "extension slots" (256) the first time
//! `eglGetProcAddress` looks it up. Slint's Skia OpenGL surface first loads
//! glow's whole function table, desktop GL included, and only then lets Skia
//! assemble its GLES interface. Vendor drivers return nothing for desktop-only
//! names, but Mesa returns an entry point for every name, so under Mesa glow
//! uses up all slots. Skia then gets nothing for the extension functions it
//! needs, e.g. `glTextureBarrierNV` when the driver advertises
//! `GL_NV_texture_barrier`, and the app cannot draw ("Could not create Skia
//! Direct Context from GL interface", with "no more slots for
//! eglGetProcAddress" before it in logcat).
//!
//! Looking Skia's extension functions up once, before Slint creates its
//! surface, gives them slots first; libEGL remembers them, and the lookups
//! that later fail are glow's desktop-only ones, which nothing here uses.

use std::ffi::{CStr, c_char, c_void};

use tracing::info;

#[link(name = "EGL")]
#[allow(unsafe_code)] // FFI declaration
unsafe extern "C" {
	fn eglGetProcAddress(procname: *const c_char) -> *const c_void;
}

/// Every extension function (name with a vendor suffix) that Skia's GLES
/// interface can look up: `GET_PROC_SUFFIX` in Skia m153's
/// `src/gpu/ganesh/gl/GrGLAssembleGLESInterfaceAutogen.cpp` (the version
/// skia-safe 0.153 builds). Core GLES functions have built-in entry points and
/// need no slot. Update the list when skia-safe moves to a new Skia milestone.
const SKIA_GLES_EXTENSION_FUNCTIONS: &[&CStr] = &[
	c"glBeginQueryEXT",
	c"glBindFragDataLocationEXT",
	c"glBindFragDataLocationIndexedEXT",
	c"glBindUniformLocationCHROMIUM",
	c"glBindVertexArrayOES",
	c"glBlendBarrierKHR",
	c"glBlendBarrierNV",
	c"glBlitFramebufferANGLE",
	c"glBlitFramebufferCHROMIUM",
	c"glBlitFramebufferNV",
	c"glClearTexImageEXT",
	c"glClearTexSubImageEXT",
	c"glClientWaitSyncAPPLE",
	c"glCopyBufferSubDataNV",
	c"glDebugMessageCallbackKHR",
	c"glDebugMessageControlKHR",
	c"glDebugMessageInsertKHR",
	c"glDeleteFencesNV",
	c"glDeleteQueriesEXT",
	c"glDeleteSyncAPPLE",
	c"glDeleteVertexArraysOES",
	c"glDiscardFramebufferEXT",
	c"glDrawArraysInstancedANGLE",
	c"glDrawArraysInstancedBaseInstanceANGLE",
	c"glDrawArraysInstancedBaseInstanceEXT",
	c"glDrawArraysInstancedEXT",
	c"glDrawElementsInstancedANGLE",
	c"glDrawElementsInstancedBaseVertexBaseInstanceANGLE",
	c"glDrawElementsInstancedBaseVertexBaseInstanceEXT",
	c"glDrawElementsInstancedEXT",
	c"glEndQueryEXT",
	c"glEndTilingQCOM",
	c"glFenceSyncAPPLE",
	c"glFinishFenceNV",
	c"glFlushMappedBufferRangeEXT",
	c"glFramebufferTexture2DMultisampleEXT",
	c"glFramebufferTexture2DMultisampleIMG",
	c"glGenFencesNV",
	c"glGenQueriesEXT",
	c"glGenVertexArraysOES",
	c"glGetDebugMessageLogKHR",
	c"glGetProgramBinaryOES",
	c"glGetQueryObjecti64vEXT",
	c"glGetQueryObjectui64vEXT",
	c"glGetQueryObjectuivEXT",
	c"glGetQueryivEXT",
	c"glInsertEventMarkerEXT",
	c"glIsSyncAPPLE",
	c"glMapBufferOES",
	c"glMapBufferRangeEXT",
	c"glMapBufferSubDataCHROMIUM",
	c"glMapTexSubImage2DCHROMIUM",
	c"glMultiDrawArraysIndirectEXT",
	c"glMultiDrawArraysInstancedBaseInstanceANGLE",
	c"glMultiDrawElementsIndirectEXT",
	c"glMultiDrawElementsInstancedBaseVertexBaseInstanceANGLE",
	c"glObjectLabelKHR",
	c"glPatchParameteriOES",
	c"glPopDebugGroupKHR",
	c"glPopGroupMarkerEXT",
	c"glProgramBinaryOES",
	c"glPushDebugGroupKHR",
	c"glPushGroupMarkerEXT",
	c"glQueryCounterEXT",
	c"glRenderbufferStorageMultisampleANGLE",
	c"glRenderbufferStorageMultisampleCHROMIUM",
	c"glResolveMultisampleFramebufferAPPLE",
	c"glSetFenceNV",
	c"glStartTilingQCOM",
	c"glTestFenceNV",
	c"glTexBufferEXT",
	c"glTexBufferOES",
	c"glTexBufferRangeEXT",
	c"glTexBufferRangeOES",
	c"glTexStorage2DEXT",
	c"glTextureBarrierNV",
	c"glUnmapBufferOES",
	c"glUnmapBufferSubDataCHROMIUM",
	c"glUnmapTexSubImage2DCHROMIUM",
	c"glVertexAttribDivisorANGLE",
	c"glVertexAttribDivisorEXT",
	c"glWaitSyncAPPLE",
	c"glWindowRectanglesEXT",
];

/// Look up [`SKIA_GLES_EXTENSION_FUNCTIONS`] so libEGL keeps their slots.
/// Call once per process, before the first window is shown.
pub fn reserve_skia_extension_slots() {
	let found = SKIA_GLES_EXTENSION_FUNCTIONS.iter().filter(|name| resolves(name)).count();
	info!(
		"EGL: {found} of {} Skia extension functions resolved ahead of Slint's surface",
		SKIA_GLES_EXTENSION_FUNCTIONS.len()
	);
}

#[allow(unsafe_code)] // the FFI call
fn resolves(name: &CStr) -> bool {
	// SAFETY: `name` is NUL-terminated; eglGetProcAddress may be the first EGL
	// call of the process and only returns an address, which is not used.
	!unsafe { eglGetProcAddress(name.as_ptr()) }.is_null()
}
