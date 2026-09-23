//! Optional local recognition backends. Register explicitly with automation::Recognizers.
//! Template matching needs no native runtime. OCR requires a local Tesseract executable
//! and language data; no model files are downloaded or cloud calls made implicitly.
mod ocr;
mod template;
pub use ocr::Tesseract;
pub use template::Templates;
