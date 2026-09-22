//! Structured recognition on one immutable frame. Model-backed algorithms are opt-in.
use crate::{Control, Error};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, future::Future, pin::Pin};

/// All geometry uses target client-area pixels, with an exclusive right/bottom edge.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}
impl Rect {
    pub fn validate(&self) -> Result<(), Error> {
        if self.x < 0
            || self.y < 0
            || self.width == 0
            || self.height == 0
            || i64::from(self.x) + i64::from(self.width) > i64::from(i32::MAX)
            || i64::from(self.y) + i64::from(self.height) > i64::from(i32::MAX)
        {
            return Err(Error::Invalid("invalid rectangle".into()));
        }
        Ok(())
    }
}

/// Recognition area: fixed client pixels or fractions of the current frame.
/// Relative edges are rounded outward so a nonempty region retains pixel coverage.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Roi {
    Fixed {
        rect: Rect,
    },
    Relative {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    },
}
impl Roi {
    pub fn validate(&self) -> Result<(), Error> {
        match *self {
            Self::Fixed { rect } => rect.validate(),
            Self::Relative {
                x,
                y,
                width,
                height,
            } => {
                if [x, y, width, height].iter().any(|v| !v.is_finite())
                    || x < 0.0
                    || y < 0.0
                    || width <= 0.0
                    || height <= 0.0
                    || x + width > 1.0
                    || y + height > 1.0
                {
                    return Err(Error::Invalid(
                        "relative ROI must be nonempty and inside 0..1".into(),
                    ));
                }
                Ok(())
            }
        }
    }
    pub fn resolve(&self, frame: &Frame) -> Result<Rect, Error> {
        self.validate()?;
        let rect = match *self {
            Self::Fixed { rect } => rect,
            Self::Relative {
                x,
                y,
                width,
                height,
            } => {
                let left = (x * f64::from(frame.width)).floor() as i32;
                let top = (y * f64::from(frame.height)).floor() as i32;
                let right = ((x + width) * f64::from(frame.width)).ceil() as u32;
                let bottom = ((y + height) * f64::from(frame.height)).ceil() as u32;
                Rect {
                    x: left,
                    y: top,
                    width: right - left as u32,
                    height: bottom - top as u32,
                }
            }
        };
        frame.region(Some(rect))
    }
}

/// Packed RGB8 screenshot; construction validates the buffer dimensions.
pub struct Frame {
    width: u32,
    height: u32,
    rgb: Vec<u8>,
}
impl Frame {
    pub fn new(width: u32, height: u32, rgb: Vec<u8>) -> Result<Self, Error> {
        let size = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(3));
        if width == 0
            || height == 0
            || width > i32::MAX as u32
            || height > i32::MAX as u32
            || size != Some(rgb.len())
        {
            return Err(Error::Invalid("invalid RGB frame".into()));
        }
        Ok(Self { width, height, rgb })
    }
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn rgb(&self) -> &[u8] {
        &self.rgb
    }
    fn recognition_region(&self, roi: Option<Roi>) -> Result<Rect, Error> {
        match roi {
            Some(roi) => roi.resolve(self),
            None => self.region(None),
        }
    }
    pub fn region(&self, roi: Option<Rect>) -> Result<Rect, Error> {
        let r = roi.unwrap_or(Rect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        });
        r.validate()?;
        if r.x as u64 + u64::from(r.width) > u64::from(self.width)
            || r.y as u64 + u64::from(r.height) > u64::from(self.height)
        {
            return Err(Error::Invalid("ROI outside frame".into()));
        }
        Ok(r)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Detection {
    pub bounds: Option<Rect>,
    pub score: Option<f64>,
    pub text: Option<String>,
    pub label: Option<String>,
    #[serde(default)]
    pub detail: Value,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecognitionResult {
    /// Empty means a normal non-match, not an operational failure.
    pub matches: Vec<Detection>,
    /// Composite recognitions retain child evidence.
    #[serde(default)]
    pub children: Vec<RecognitionResult>,
}
impl RecognitionResult {
    pub fn matched(&self) -> bool {
        !self.matches.is_empty()
    }
    fn region(rect: Rect, score: f64) -> Self {
        Self {
            matches: vec![Detection {
                bounds: Some(rect),
                score: Some(score),
                text: None,
                label: None,
                detail: Value::Null,
            }],
            children: vec![],
        }
    }
}

/// Built-in kinds define dispatch contracts, not a claim that a model is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Algorithm {
    TemplateMatch,
    FeatureMatch,
    Ocr,
    NeuralNetworkClassify,
    NeuralNetworkDetect,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Recognition {
    DirectHit {
        roi: Option<Roi>,
    },
    ColorMatch {
        roi: Option<Roi>,
        lower: [u8; 3],
        upper: [u8; 3],
        min_ratio: f64,
    },
    /// Parameters are owned and validated by the registered algorithm backend.
    Algorithm {
        algorithm: Algorithm,
        roi: Option<Roi>,
        parameters: Value,
    },
    Custom {
        name: String,
        roi: Option<Roi>,
        parameters: Value,
    },
    And {
        conditions: Vec<Recognition>,
    },
    Or {
        conditions: Vec<Recognition>,
    },
}
impl Recognition {
    pub fn validate(&self) -> Result<(), Error> {
        match self {
            Self::And { conditions } | Self::Or { conditions } => {
                if conditions.is_empty() {
                    return Err(Error::Invalid("empty composite recognition".into()));
                }
                for condition in conditions {
                    condition.validate()?;
                }
                return Ok(());
            }
            Self::ColorMatch {
                lower,
                upper,
                min_ratio,
                ..
            } => {
                if !(0.0..=1.0).contains(min_ratio) || lower.iter().zip(upper).any(|(a, b)| a > b) {
                    return Err(Error::Invalid("invalid color range or ratio".into()));
                }
            }
            Self::Custom { name, .. } if name.trim().is_empty() => {
                return Err(Error::Invalid("empty recognition name".into()));
            }
            _ => {}
        }
        let roi = match self {
            Self::DirectHit { roi }
            | Self::ColorMatch { roi, .. }
            | Self::Algorithm { roi, .. }
            | Self::Custom { roi, .. } => roi,
            _ => unreachable!(),
        };
        if let Some(roi) = roi {
            roi.validate()?;
        }
        Ok(())
    }
}

pub type RecognitionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<RecognitionResult, Error>> + 'a>>;
/// Extensions receive the full immutable frame and a validated ROI in client pixels.
/// Returned bounds must also be in client pixels. Futures must cooperate with cancellation.
pub trait Recognizer {
    fn recognize<'a>(
        &'a mut self,
        frame: &'a Frame,
        roi: Rect,
        parameters: &'a Value,
        control: &'a Control,
    ) -> RecognitionFuture<'a>;
}
#[derive(Default)]
pub struct Recognizers {
    algorithms: BTreeMap<Algorithm, Box<dyn Recognizer>>,
    custom: BTreeMap<String, Box<dyn Recognizer>>,
}
impl Recognizers {
    pub fn register_algorithm(
        &mut self,
        kind: Algorithm,
        recognizer: impl Recognizer + 'static,
    ) -> Result<(), Error> {
        if self.algorithms.contains_key(&kind) {
            return Err(Error::Invalid(format!("duplicate algorithm: {kind:?}")));
        }
        self.algorithms.insert(kind, Box::new(recognizer));
        Ok(())
    }
    pub fn register_custom(
        &mut self,
        name: String,
        recognizer: impl Recognizer + 'static,
    ) -> Result<(), Error> {
        if name.trim().is_empty() || self.custom.contains_key(&name) {
            return Err(Error::Invalid(format!(
                "duplicate or empty recognizer: {name}"
            )));
        }
        self.custom.insert(name, Box::new(recognizer));
        Ok(())
    }
    pub fn supports(&self, recognition: &Recognition) -> bool {
        match recognition {
            Recognition::Algorithm { algorithm, .. } => self.algorithms.contains_key(algorithm),
            Recognition::Custom { name, .. } => self.custom.contains_key(name),
            Recognition::And { conditions } | Recognition::Or { conditions } => {
                conditions.iter().all(|r| self.supports(r))
            }
            _ => true,
        }
    }
    pub fn recognize<'a>(
        &'a mut self,
        recognition: &'a Recognition,
        frame: &'a Frame,
        control: &'a Control,
    ) -> RecognitionFuture<'a> {
        Box::pin(async move {
            control.check()?;
            recognition.validate()?;
            let result = match recognition {
                Recognition::DirectHit { roi } => {
                    RecognitionResult::region(frame.recognition_region(*roi)?, 1.0)
                }
                Recognition::ColorMatch {
                    roi,
                    lower,
                    upper,
                    min_ratio,
                } => {
                    let rect = frame.recognition_region(*roi)?;
                    let mut count = 0u64;
                    for y in rect.y as u32..rect.y as u32 + rect.height {
                        control.check()?;
                        for x in rect.x as u32..rect.x as u32 + rect.width {
                            let offset = (y as usize * frame.width as usize + x as usize) * 3;
                            if (0..3).all(|c| {
                                frame.rgb[offset + c] >= lower[c]
                                    && frame.rgb[offset + c] <= upper[c]
                            }) {
                                count += 1;
                            }
                        }
                        if y % 32 == 0 {
                            tokio::task::yield_now().await;
                        }
                    }
                    let ratio =
                        count as f64 / (u64::from(rect.width) * u64::from(rect.height)) as f64;
                    if ratio >= *min_ratio {
                        RecognitionResult::region(rect, ratio)
                    } else {
                        RecognitionResult::default()
                    }
                }
                Recognition::Algorithm {
                    algorithm,
                    roi,
                    parameters,
                } => {
                    self.algorithms
                        .get_mut(algorithm)
                        .ok_or_else(|| Error::Unsupported(format!("{algorithm:?}")))?
                        .recognize(frame, frame.recognition_region(*roi)?, parameters, control)
                        .await?
                }
                Recognition::Custom {
                    name,
                    roi,
                    parameters,
                } => {
                    self.custom
                        .get_mut(name)
                        .ok_or_else(|| Error::Unsupported(name.clone()))?
                        .recognize(frame, frame.recognition_region(*roi)?, parameters, control)
                        .await?
                }
                Recognition::And { conditions } | Recognition::Or { conditions } => {
                    let all = matches!(recognition, Recognition::And { .. });
                    let mut result = RecognitionResult::default();
                    for condition in conditions {
                        let child = self.recognize(condition, frame, control).await?;
                        let matched = child.matched();
                        if matched && result.matches.is_empty() {
                            result.matches = child.matches.clone();
                        }
                        result.children.push(child);
                        if all && !matched {
                            result.matches.clear();
                            break;
                        }
                        if !all && matched {
                            break;
                        }
                    }
                    result
                }
            };
            for detection in &result.matches {
                if let Some(bounds) = detection.bounds {
                    frame.region(Some(bounds))?;
                }
                if detection.score.is_some_and(|s| !s.is_finite()) {
                    return Err(Error::Backend("non-finite recognition score".into()));
                }
            }
            Ok(result)
        })
    }
}
