//! Shared utilities for the `evo-plugins-audio` brand-neutral plugin
//! commons. Workspace-internal; not part of any plugin's public
//! surface and not shipped as a separate artefact.
//!
//! Plugins in this workspace consume this crate when they need shared
//! types or helpers: path normalisation, library scanning, common
//! error shapes, cross-plugin trace helpers. Anything that genuinely
//! lives outside the audio domain belongs in `evo-core`'s SDK, not
//! here.
//!
//! This crate is empty until the first plugin migration introduces
//! shared code that warrants extraction. The shape mirrors
//! `evo-volumio-library` in the Volumio distribution.
