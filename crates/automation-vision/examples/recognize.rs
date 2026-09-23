//! cargo run -p automation-vision --example recognize -- <image.png> [language]
use automation::{Control, recognition::*};
use automation_vision::Tesseract;
use serde_json::json;
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("provide an image path")?;
    let language = args.next().unwrap_or_else(|| "eng".into());
    let image = image::open(path)?.into_rgb8();
    let frame = Frame::new(image.width(), image.height(), image.into_raw())?;
    let mut recognizers = Recognizers::default();
    recognizers.register_algorithm(Algorithm::Ocr, Tesseract::default())?;
    let result = recognizers
        .recognize(
            &Recognition::Algorithm {
                algorithm: Algorithm::Ocr,
                roi: None,
                parameters: json!({"language":language}),
            },
            &frame,
            &Control::default(),
        )
        .await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
