//! Optional local recognition backends. Register explicitly with automation::Recognizers.
//! Template matching needs no native runtime. OCR uses a resident ocr-rs / MNN engine
//! and explicit model paths; no models are downloaded or cloud calls made at runtime.
mod ocr;
mod template;
pub use ocr::Ocr;
pub use template::Templates;
