use automation::{Control, Error, recognition::*};
use automation_vision::{Ocr, Templates};
use image::{Rgba, RgbaImage};
use serde_json::json;

#[tokio::test]
async fn template_finds_two_icons_in_roi_and_preserves_client_coordinates() {
    let mut templates = Templates::default();
    let mut icon = RgbaImage::new(2, 2);
    icon.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
    icon.put_pixel(1, 0, Rgba([0, 255, 0, 255]));
    icon.put_pixel(0, 1, Rgba([0, 0, 255, 255]));
    icon.put_pixel(1, 1, Rgba([255, 255, 255, 0]));
    templates.insert("icon", icon).unwrap();
    let mut rgb = vec![0; 12 * 6 * 3];
    for x in [3, 8] {
        for (dx, dy, color) in [
            (0, 0, [255, 0, 0]),
            (1, 0, [0, 255, 0]),
            (0, 1, [0, 0, 255]),
        ] {
            let start = ((2 + dy) * 12 + x + dx) * 3;
            rgb[start..start + 3].copy_from_slice(&color);
        }
    }
    let frame = Frame::new(12, 6, rgb).unwrap();
    let mut rs = Recognizers::default();
    rs.register_algorithm(Algorithm::TemplateMatch, templates)
        .unwrap();
    let recognition = Recognition::Algorithm {
        algorithm: Algorithm::TemplateMatch,
        roi: Some(Roi::Fixed {
            rect: Rect {
                x: 2,
                y: 1,
                width: 9,
                height: 4,
            },
        }),
        parameters: json!({"template":"icon","threshold":1.0,"max_results":2}),
    };
    let result = rs
        .recognize(&recognition, &frame, &Control::default())
        .await
        .unwrap();
    assert_eq!(result.matches.len(), 2);
    assert_eq!(result.matches[0].bounds.unwrap().x, 3);
    assert_eq!(result.matches[1].bounds.unwrap().x, 8);
    assert_eq!(result.matches[0].score, Some(1.0));
}
#[tokio::test]
async fn missing_template_is_an_error_but_larger_template_is_a_nonmatch() {
    let mut templates = Templates::default();
    templates
        .insert("large", RgbaImage::from_pixel(3, 3, Rgba([255, 0, 0, 255])))
        .unwrap();
    let frame = Frame::new(1, 1, vec![0, 0, 0]).unwrap();
    let roi = Rect {
        x: 0,
        y: 0,
        width: 1,
        height: 1,
    };
    let control = Control::default();
    assert!(matches!(
        templates
            .recognize(&frame, roi, &json!({"template":"missing"}), &control)
            .await,
        Err(Error::Unsupported(_))
    ));
    assert!(
        !templates
            .recognize(&frame, roi, &json!({"template":"large"}), &control)
            .await
            .unwrap()
            .matched()
    );
}
#[tokio::test]
async fn missing_ocr_models_are_an_initialization_error() {
    assert!(matches!(
        Ocr::new("missing-det.mnn", "missing-rec.mnn", "missing-keys.txt").await,
        Err(Error::Backend(_))
    ));
}

/// Runs actual embedded models, without external files or downloads.
#[tokio::test]
async fn real_ocr_reads_generated_text_and_returns_boxes() {
    let patterns = [
        [
            "11111", "00100", "00100", "00100", "00100", "00100", "00100",
        ],
        [
            "11111", "10000", "10000", "11110", "10000", "10000", "11111",
        ],
        [
            "01111", "10000", "10000", "01110", "00001", "00001", "11110",
        ],
        [
            "11111", "00100", "00100", "00100", "00100", "00100", "00100",
        ],
    ];
    let (width, height) = (280, 110);
    let mut rgb = vec![255; width * height * 3];
    for (letter, rows) in patterns.iter().enumerate() {
        for (y, row) in rows.iter().enumerate() {
            for (x, pixel) in row.bytes().enumerate() {
                if pixel != b'1' {
                    continue;
                }
                for dy in 0..10 {
                    for dx in 0..10 {
                        let index =
                            ((20 + y * 10 + dy) * width + 20 + letter * 60 + x * 10 + dx) * 3;
                        rgb[index..index + 3].fill(0);
                    }
                }
            }
        }
    }
    let frame = Frame::new(width as u32, height as u32, rgb).unwrap();
    let mut ocr = Ocr::bundled().await.unwrap();
    let result = ocr
        .recognize(
            &frame,
            Rect {
                x: 0,
                y: 0,
                width: width as u32,
                height: height as u32,
            },
            &json!({}),
            &Control::default(),
        )
        .await
        .unwrap();
    assert!(
        result
            .matches
            .iter()
            .any(|m| m.text.as_deref() == Some("TEST")),
        "{result:?}"
    );
    assert!(
        result
            .matches
            .iter()
            .all(|m| m.bounds.is_some() && m.score.is_some())
    );
}
