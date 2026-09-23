use automation::{
    Control, Error,
    recognition::{Detection, Frame, RecognitionFuture, RecognitionResult, Recognizer, Rect},
};
use image::{DynamicImage, RgbImage};
use serde::Deserialize;
use serde_json::Value;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

/// One resident CPU engine on a dedicated worker. Models load once per instance.
/// Cancellation stops waiting; native inference finishes before another request starts.
pub struct Ocr {
    sender: mpsc::Sender<Job>,
    capacity: Arc<Semaphore>,
    timeout: Duration,
}

struct Job {
    image: DynamicImage,
    reply: oneshot::Sender<Result<Vec<Detection>, Error>>,
    _permit: OwnedSemaphorePermit,
}

impl Ocr {
    /// Loads the embedded PP-OCRv6 small detection, recognition and dictionary assets.
    pub async fn bundled() -> Result<Self, Error> {
        Self::from_engine(|| {
            ocr_rs::OcrEngine::from_bytes(
                include_bytes!("../models/pp-ocrv6-small/PP-OCRv6_small_det.mnn"),
                include_bytes!("../models/pp-ocrv6-small/PP-OCRv6_small_rec.mnn"),
                include_bytes!("../models/pp-ocrv6-small/ppocr_keys_v6_small.txt"),
                None,
            )
            .map_err(|e| Error::Backend(e.to_string()))
        })
        .await
    }

    /// Model and dictionary files must match. No runtime downloads are performed.
    pub async fn new(
        det: impl Into<PathBuf>,
        rec: impl Into<PathBuf>,
        dictionary: impl Into<PathBuf>,
    ) -> Result<Self, Error> {
        let (det, rec, dictionary) = (det.into(), rec.into(), dictionary.into());
        Self::from_engine(move || {
            ocr_rs::OcrEngine::new(det, rec, dictionary, None)
                .map_err(|e| Error::Backend(e.to_string()))
        })
        .await
    }

    async fn from_engine(
        initialize: impl FnOnce() -> Result<ocr_rs::OcrEngine, Error> + Send + 'static,
    ) -> Result<Self, Error> {
        Self::start(move || {
            let engine = initialize()?;
            Ok(move |image: DynamicImage| {
                engine
                    .recognize(&image)
                    .map_err(|e| Error::Backend(e.to_string()))?
                    .into_iter()
                    .map(|item| {
                        let rect = item.bbox.rect;
                        Ok(Detection {
                            bounds: Some(Rect {
                                x: rect.left(),
                                y: rect.top(),
                                width: rect.width(),
                                height: rect.height(),
                            }),
                            score: Some(f64::from(item.confidence)),
                            text: Some(item.text),
                            label: None,
                            detail: Value::Null,
                        })
                    })
                    .collect()
            })
        })
        .await
    }

    async fn start<F, R>(initialize: F) -> Result<Self, Error>
    where
        F: FnOnce() -> Result<R, Error> + Send + 'static,
        R: FnMut(DynamicImage) -> Result<Vec<Detection>, Error> + 'static,
    {
        let (sender, mut receiver) = mpsc::channel::<Job>(1);
        let (ready, initialized) = oneshot::channel();
        std::thread::Builder::new()
            .name("actweave-ocr".into())
            .spawn(move || {
                let mut recognize = match initialize() {
                    Ok(engine) => engine,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                if ready.send(Ok(())).is_err() {
                    return;
                }
                while let Some(job) = receiver.blocking_recv() {
                    if !job.reply.is_closed() {
                        let result = recognize(job.image);
                        let _ = job.reply.send(result);
                    }
                }
            })
            .map_err(|e| Error::Backend(e.to_string()))?;
        initialized
            .await
            .map_err(|_| Error::Backend("OCR worker stopped during initialization".into()))??;
        Ok(Self {
            sender,
            capacity: Arc::new(Semaphore::new(1)),
            timeout: Duration::from_secs(15),
        })
    }

    /// Includes time waiting for a previous cancelled inference to finish.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    async fn run(
        &self,
        frame: &Frame,
        roi: Rect,
        p: &Parameters,
    ) -> Result<RecognitionResult, Error> {
        let permit = self
            .capacity
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Backend("OCR worker unavailable".into()))?;
        let mut rgb = Vec::with_capacity(roi.width as usize * roi.height as usize * 3);
        for y in roi.y as u32..roi.y as u32 + roi.height {
            let start = (y as usize * frame.width() as usize + roi.x as usize) * 3;
            rgb.extend_from_slice(&frame.rgb()[start..start + roi.width as usize * 3]);
        }
        let image = RgbImage::from_raw(roi.width, roi.height, rgb)
            .ok_or_else(|| Error::Invalid("invalid OCR crop".into()))?;
        let (reply, response) = oneshot::channel();
        self.sender
            .send(Job {
                image: DynamicImage::ImageRgb8(image),
                reply,
                _permit: permit,
            })
            .await
            .map_err(|_| Error::Backend("OCR worker stopped".into()))?;
        let matches = response
            .await
            .map_err(|_| Error::Backend("OCR worker stopped".into()))??;
        convert(matches, roi, p)
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Parameters {
    #[serde(default)]
    min_confidence: f64,
    #[serde(default)]
    contains: Option<String>,
}

fn convert(matches: Vec<Detection>, roi: Rect, p: &Parameters) -> Result<RecognitionResult, Error> {
    let mut result = RecognitionResult::default();
    for mut item in matches {
        let score = item.score.unwrap_or(f64::NAN);
        let bounds = item
            .bounds
            .as_mut()
            .ok_or_else(|| Error::Backend("OCR result missing bounds".into()))?;
        if !(0.0..=1.0).contains(&score)
            || bounds.x < 0
            || bounds.y < 0
            || bounds.width == 0
            || bounds.height == 0
            || bounds.x as u64 + u64::from(bounds.width) > u64::from(roi.width)
            || bounds.y as u64 + u64::from(bounds.height) > u64::from(roi.height)
        {
            return Err(Error::Backend(
                "invalid OCR result geometry or confidence".into(),
            ));
        }
        let text = item.text.as_deref().unwrap_or("").trim();
        if text.is_empty()
            || score < p.min_confidence
            || p.contains
                .as_ref()
                .is_some_and(|needle| !text.contains(needle))
        {
            continue;
        }
        bounds.x += roi.x;
        bounds.y += roi.y;
        result.matches.push(item);
    }
    Ok(result)
}

impl Recognizer for Ocr {
    fn recognize<'a>(
        &'a mut self,
        frame: &'a Frame,
        roi: Rect,
        parameters: &'a Value,
        control: &'a Control,
    ) -> RecognitionFuture<'a> {
        Box::pin(async move {
            control.check()?;
            frame.region(Some(roi))?;
            let p: Parameters = serde_json::from_value(parameters.clone())
                .map_err(|e| Error::Invalid(e.to_string()))?;
            if !(0.0..=1.0).contains(&p.min_confidence) || self.timeout.is_zero() {
                return Err(Error::Invalid("invalid OCR confidence or timeout".into()));
            }
            tokio::select! {
                biased;
                _ = control.cancelled() => Err(Error::Cancelled),
                result = tokio::time::timeout(self.timeout, self.run(frame, roi, &p)) => result.map_err(|_| Error::TimedOut)?,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn timeout_does_not_queue_more_inference_and_engine_is_reused() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let (release, gate) = std::sync::mpsc::channel();
        let mut ocr = Ocr::start(move || {
            Ok(move |_: DynamicImage| {
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    gate.recv().unwrap();
                }
                Ok(vec![])
            })
        })
        .await
        .unwrap()
        .with_timeout(Duration::from_millis(30));
        let frame = Frame::new(1, 1, vec![255; 3]).unwrap();
        let roi = Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        let p = json!({});
        let control = Control::default();
        for _ in 0..2 {
            assert!(matches!(
                ocr.recognize(&frame, roi, &p, &control).await,
                Err(Error::TimedOut)
            ));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        release.send(()).unwrap();
        ocr.timeout = Duration::from_secs(2);
        assert!(
            !ocr.recognize(&frame, roi, &p, &control)
                .await
                .unwrap()
                .matched()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cancellation_keeps_task_control_responsive_during_inference() {
        let (release, gate) = std::sync::mpsc::channel();
        let (started, mut running) = oneshot::channel();
        let mut started = Some(started);
        let mut ocr = Ocr::start(move || {
            Ok(move |_: DynamicImage| {
                if let Some(started) = started.take() {
                    let _ = started.send(());
                }
                gate.recv().unwrap();
                Ok(vec![])
            })
        })
        .await
        .unwrap();
        let frame = Frame::new(1, 1, vec![255; 3]).unwrap();
        let control = Control::default();
        let p = json!({});
        let request = ocr.recognize(
            &frame,
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            &p,
            &control,
        );
        let cancel = async {
            (&mut running).await.unwrap();
            control.cancel();
        };
        let (result, ()) = tokio::join!(request, cancel);
        release.send(()).unwrap();
        assert!(matches!(result, Err(Error::Cancelled)));
    }

    #[test]
    fn converts_crop_coordinates_and_filters_text() {
        let detection = |text: &str| Detection {
            bounds: Some(Rect {
                x: 2,
                y: 3,
                width: 20,
                height: 10,
            }),
            score: Some(0.95),
            text: Some(text.into()),
            label: None,
            detail: Value::Null,
        };
        let roi = Rect {
            x: 100,
            y: 200,
            width: 80,
            height: 40,
        };
        let p = Parameters {
            min_confidence: 0.8,
            contains: Some("继续".into()),
        };
        let result = convert(vec![detection("继续"), detection("返回")], roi, &p).unwrap();
        assert_eq!(result.matches.len(), 1);
        let bounds = result.matches[0].bounds.unwrap();
        assert_eq!((bounds.x, bounds.y), (102, 203));
        let mut invalid = detection("继续");
        invalid.score = Some(f64::NAN);
        assert!(convert(vec![invalid], roi, &p).is_err());
    }
}
