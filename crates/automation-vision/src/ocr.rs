use automation::{
    Control, Error,
    recognition::{Detection, Frame, RecognitionFuture, RecognitionResult, Recognizer, Rect},
};
use serde::Deserialize;
use serde_json::Value;
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command};

/// Local process backend. Dropping a recognition kills its child process.
pub struct Tesseract {
    executable: PathBuf,
    data_dir: Option<PathBuf>,
    timeout: Duration,
}
impl Default for Tesseract {
    fn default() -> Self {
        Self::new("tesseract")
    }
}
impl Tesseract {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            data_dir: None,
            timeout: Duration::from_secs(15),
        }
    }
    pub fn with_data_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.data_dir = Some(path.into());
        self
    }
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
    async fn run(
        &self,
        frame: &Frame,
        roi: Rect,
        parameters: &Parameters,
    ) -> Result<RecognitionResult, Error> {
        frame.region(Some(roi))?;
        let mut image = format!("P6\n{} {}\n255\n", roi.width, roi.height).into_bytes();
        for y in roi.y as u32..roi.y as u32 + roi.height {
            let start = (y as usize * frame.width() as usize + roi.x as usize) * 3;
            image.extend_from_slice(&frame.rgb()[start..start + roi.width as usize * 3]);
        }
        let mut command = Command::new(&self.executable);
        command
            .args([
                "stdin",
                "stdout",
                "-l",
                &parameters.language,
                "--psm",
                &parameters.psm.to_string(),
                "tsv",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(dir) = &self.data_dir {
            command.env("TESSDATA_PREFIX", dir);
        }
        let mut child = command
            .spawn()
            .map_err(|e| Error::Backend(format!("cannot start local Tesseract: {e}")))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Backend("missing OCR input pipe".into()))?;
        let write = async move {
            stdin.write_all(&image).await?;
            stdin.shutdown().await
        };
        let (_, output) = tokio::try_join!(write, child.wait_with_output())
            .map_err(|e| Error::Backend(e.to_string()))?;
        if !output.status.success() {
            return Err(Error::Backend(format!(
                "Tesseract failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        let tsv = String::from_utf8(output.stdout).map_err(|e| Error::Backend(e.to_string()))?;
        parse_tsv(&tsv, roi, parameters)
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    #[serde(default = "language")]
    language: String,
    #[serde(default = "psm")]
    psm: u8,
    #[serde(default)]
    min_confidence: f64,
    /// Optional substring filter; matching remains local and deterministic.
    #[serde(default)]
    contains: Option<String>,
}
fn language() -> String {
    "eng".into()
}
fn psm() -> u8 {
    6
}
fn parse_tsv(tsv: &str, roi: Rect, p: &Parameters) -> Result<RecognitionResult, Error> {
    if !tsv.starts_with("level\t") {
        return Err(Error::Backend("invalid OCR TSV header".into()));
    }
    let mut result = RecognitionResult::default();
    for line in tsv.lines().skip(1) {
        let fields: Vec<_> = line.splitn(12, '\t').collect();
        if fields.first() != Some(&"5") {
            continue;
        }
        if fields.len() != 12 {
            return Err(Error::Backend("invalid OCR word row".into()));
        }
        let number = |index: usize| {
            fields[index]
                .parse::<u32>()
                .map_err(|_| Error::Backend("invalid OCR geometry".into()))
        };
        let (x, y, width, height) = (number(6)?, number(7)?, number(8)?, number(9)?);
        if width == 0
            || height == 0
            || u64::from(x) + u64::from(width) > u64::from(roi.width)
            || u64::from(y) + u64::from(height) > u64::from(roi.height)
        {
            return Err(Error::Backend("OCR result outside crop".into()));
        }
        let confidence = fields[10]
            .parse::<f64>()
            .map_err(|_| Error::Backend("invalid OCR confidence".into()))?
            / 100.0;
        if !(0.0..=1.0).contains(&confidence) {
            return Err(Error::Backend("invalid OCR confidence".into()));
        }
        let text = fields[11].trim();
        if text.is_empty()
            || confidence < p.min_confidence
            || p.contains
                .as_ref()
                .is_some_and(|needle| !text.contains(needle))
        {
            continue;
        }
        result.matches.push(Detection {
            bounds: Some(Rect {
                x: roi.x + x as i32,
                y: roi.y + y as i32,
                width,
                height,
            }),
            score: Some(confidence),
            text: Some(text.into()),
            label: None,
            detail: Value::Null,
        });
    }
    Ok(result)
}
impl Recognizer for Tesseract {
    fn recognize<'a>(
        &'a mut self,
        frame: &'a Frame,
        roi: Rect,
        parameters: &'a Value,
        control: &'a Control,
    ) -> RecognitionFuture<'a> {
        Box::pin(async move {
            let parameters: Parameters = serde_json::from_value(parameters.clone())
                .map_err(|e| Error::Invalid(e.to_string()))?;
            if parameters.language.is_empty()
                || !parameters
                    .language
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+'))
                || !(3..=13).contains(&parameters.psm)
                || !(0.0..=1.0).contains(&parameters.min_confidence)
                || self.timeout.is_zero()
            {
                return Err(Error::Invalid(
                    "invalid OCR language, psm, confidence or timeout".into(),
                ));
            }
            control.check()?;
            tokio::select! {
                biased;
                _=control.cancelled()=>Err(Error::Cancelled),
                result=tokio::time::timeout(self.timeout,self.run(frame,roi,&parameters))=>result.map_err(|_|Error::TimedOut)?,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roi() -> Rect {
        Rect {
            x: 100,
            y: 200,
            width: 80,
            height: 40,
        }
    }

    #[test]
    fn words_are_filtered_and_translated_to_frame_coordinates() {
        let parameters =
            serde_json::from_value(json!({"contains":"继续", "min_confidence":0.8})).unwrap();
        let tsv = "level\tpage_num\n5\t1\t1\t1\t1\t1\t2\t3\t20\t10\t95\t继续\n5\t1\t1\t1\t1\t2\t25\t3\t20\t10\t70\t继续\n5\t1\t1\t1\t1\t3\t50\t3\t20\t10\t99\t返回\n";
        let result = parse_tsv(tsv, roi(), &parameters).unwrap();
        assert_eq!(result.matches.len(), 1);
        let bounds = result.matches[0].bounds.unwrap();
        assert_eq!(
            (bounds.x, bounds.y, bounds.width, bounds.height),
            (102, 203, 20, 10)
        );
        assert_eq!(result.matches[0].score, Some(0.95));
    }

    #[test]
    fn malformed_output_is_distinct_from_no_words() {
        let parameters = serde_json::from_value(json!({})).unwrap();
        assert!(
            !parse_tsv("level\tpage_num\n", roi(), &parameters)
                .unwrap()
                .matched()
        );
        for tsv in [
            "not TSV",
            "level\tpage_num\n5\t1",
            "level\tpage_num\n5\t1\t1\t1\t1\t1\t79\t3\t20\t10\t95\ttext",
            "level\tpage_num\n5\t1\t1\t1\t1\t1\t2\t3\t20\t10\tNaN\ttext",
        ] {
            assert!(matches!(
                parse_tsv(tsv, roi(), &parameters),
                Err(Error::Backend(_))
            ));
        }
    }
}
