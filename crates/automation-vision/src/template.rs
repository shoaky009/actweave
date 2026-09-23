use automation::{
    Control, Error,
    recognition::{Detection, Frame, RecognitionFuture, RecognitionResult, Recognizer, Rect},
};
use image::RgbaImage;
use serde::Deserialize;
use serde_json::Value;
use std::{collections::BTreeMap, path::Path};

/// Preloaded fixed-scale UI templates. Score is 1 - mean absolute RGB error / 255;
/// it is not ZNCC and thresholds are not interchangeable with OpenCV correlation.
#[derive(Default)]
pub struct Templates {
    images: BTreeMap<String, RgbaImage>,
}
impl Templates {
    pub fn load(&mut self, name: &str, path: impl AsRef<Path>) -> Result<(), Error> {
        let image = image::open(path)
            .map_err(|e| Error::Backend(e.to_string()))?
            .to_rgba8();
        self.insert(name, image)
    }
    /// Alpha below 128 excludes a pixel from scoring.
    pub fn insert(&mut self, name: &str, image: RgbaImage) -> Result<(), Error> {
        if name.is_empty()
            || self.images.contains_key(name)
            || image.width() == 0
            || image.height() == 0
            || !image.pixels().any(|p| p[3] >= 128)
        {
            return Err(Error::Invalid(
                "empty/duplicate template or empty mask".into(),
            ));
        }
        self.images.insert(name.into(), image);
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    template: String,
    #[serde(default = "threshold")]
    threshold: f64,
    #[serde(default = "max_results")]
    max_results: usize,
}
fn threshold() -> f64 {
    0.9
}
fn max_results() -> usize {
    1
}
fn overlaps(a: Rect, b: Rect) -> bool {
    i64::from(a.x) < i64::from(b.x) + i64::from(b.width)
        && i64::from(b.x) < i64::from(a.x) + i64::from(a.width)
        && i64::from(a.y) < i64::from(b.y) + i64::from(b.height)
        && i64::from(b.y) < i64::from(a.y) + i64::from(a.height)
}
impl Recognizer for Templates {
    fn recognize<'a>(
        &'a mut self,
        frame: &'a Frame,
        roi: Rect,
        parameters: &'a Value,
        control: &'a Control,
    ) -> RecognitionFuture<'a> {
        Box::pin(async move {
            control.check()?;
            let parameters: Parameters = serde_json::from_value(parameters.clone())
                .map_err(|e| Error::Invalid(e.to_string()))?;
            if !(0.0..=1.0).contains(&parameters.threshold)
                || !(1..=100).contains(&parameters.max_results)
            {
                return Err(Error::Invalid(
                    "invalid template threshold or result limit".into(),
                ));
            }
            let template = self.images.get(&parameters.template).ok_or_else(|| {
                Error::Unsupported(format!("template {} not loaded", parameters.template))
            })?;
            frame.region(Some(roi))?;
            let mut result = RecognitionResult::default();
            if template.width() > roi.width || template.height() > roi.height {
                return Ok(result);
            }
            let samples: Vec<_> = template
                .enumerate_pixels()
                .filter(|(_, _, p)| p[3] >= 128)
                .collect();
            let mut work = 0usize;
            for y in roi.y as u32..=roi.y as u32 + roi.height - template.height() {
                for x in roi.x as u32..=roi.x as u32 + roi.width - template.width() {
                    let mut error = 0u64;
                    for (dx, dy, pixel) in &samples {
                        let index =
                            (((y + dy) as usize * frame.width() as usize) + (x + dx) as usize) * 3;
                        for c in 0..3 {
                            error += u64::from(frame.rgb()[index + c].abs_diff(pixel[c]));
                        }
                        work += 1;
                        if work.is_multiple_of(4096) {
                            control.check()?;
                            tokio::task::yield_now().await;
                        }
                    }
                    let score = 1.0 - error as f64 / (samples.len() as f64 * 3.0 * 255.0);
                    if score < parameters.threshold {
                        continue;
                    }
                    let bounds = Rect {
                        x: x as i32,
                        y: y as i32,
                        width: template.width(),
                        height: template.height(),
                    };
                    if result.matches.iter().any(|d| {
                        d.bounds.is_some_and(|b| overlaps(b, bounds))
                            && d.score.unwrap_or(0.0) >= score
                    }) {
                        continue;
                    }
                    result
                        .matches
                        .retain(|d| !d.bounds.is_some_and(|b| overlaps(b, bounds)));
                    result.matches.push(Detection {
                        bounds: Some(bounds),
                        score: Some(score),
                        text: None,
                        label: Some(parameters.template.clone()),
                        detail: Value::Null,
                    });
                    result.matches.sort_by(|a, b| {
                        b.score
                            .partial_cmp(&a.score)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                    result.matches.truncate(parameters.max_results);
                }
            }
            control.check()?;
            Ok(result)
        })
    }
}
