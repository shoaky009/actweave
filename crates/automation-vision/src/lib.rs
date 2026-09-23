//! Optional local recognition backends. Register explicitly with automation::Recognizers.
//! Template matching needs no native runtime. OCR uses a resident ocr-rs / MNN engine
//! with bundled PP-OCRv6 small or explicit model paths; no runtime model downloads or cloud calls.
mod ocr;
mod template;
pub use ocr::Ocr;
pub use template::Templates;
