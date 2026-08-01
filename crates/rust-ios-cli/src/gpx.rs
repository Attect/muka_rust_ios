use std::path::Path;

use anyhow::{Context, anyhow, bail};
use chrono::DateTime;
use rust_ios_protocol::LocationSimulationPoint;
use serde::Deserialize;

const MAX_GPX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TRACK_POINTS: usize = 10_000;

#[derive(Debug, Deserialize)]
struct GpxDocument {
    #[serde(rename = "trk", default)]
    tracks: Vec<GpxTrack>,
}

#[derive(Debug, Deserialize)]
struct GpxTrack {
    #[serde(rename = "trkseg", default)]
    segments: Vec<GpxTrackSegment>,
}

#[derive(Debug, Deserialize)]
struct GpxTrackSegment {
    #[serde(rename = "trkpt", default)]
    points: Vec<GpxTrackPoint>,
}

#[derive(Debug, Deserialize)]
struct GpxTrackPoint {
    #[serde(rename = "@lat")]
    latitude: String,
    #[serde(rename = "@lon")]
    longitude: String,
    #[serde(default)]
    time: Option<String>,
}

pub(crate) fn parse(path: &Path) -> anyhow::Result<Vec<LocationSimulationPoint>> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("cannot inspect GPX file {}", path.display()))?;
    if !metadata.is_file() {
        bail!("GPX path {} is not a regular file", path.display());
    }
    if metadata.len() > MAX_GPX_BYTES {
        bail!(
            "GPX file {} is {} bytes; maximum is {MAX_GPX_BYTES}",
            path.display(),
            metadata.len()
        );
    }
    let bytes =
        std::fs::read(path).with_context(|| format!("cannot read GPX file {}", path.display()))?;
    if bytes.len() as u64 > MAX_GPX_BYTES {
        bail!(
            "GPX file {} changed while reading and exceeds the {MAX_GPX_BYTES}-byte maximum",
            path.display()
        );
    }
    let document: GpxDocument = quick_xml::de::from_reader(bytes.as_slice())
        .with_context(|| format!("GPX file {} is not valid XML", path.display()))?;

    let raw_points = document
        .tracks
        .into_iter()
        .flat_map(|track| track.segments)
        .flat_map(|segment| segment.points)
        .collect::<Vec<_>>();
    if raw_points.is_empty() {
        bail!("GPX file {} contains no track points", path.display());
    }
    if raw_points.len() > MAX_TRACK_POINTS {
        bail!(
            "GPX file {} contains {} track points; maximum is {MAX_TRACK_POINTS}",
            path.display(),
            raw_points.len()
        );
    }

    let mut previous_timestamp_ms = None;
    raw_points
        .into_iter()
        .enumerate()
        .map(|(index, point)| {
            let latitude = parse_coordinate(&point.latitude, -90.0, 90.0, "latitude", index)?;
            let longitude = parse_coordinate(&point.longitude, -180.0, 180.0, "longitude", index)?;
            let timestamp_ms = point
                .time
                .as_deref()
                .map(|value| {
                    DateTime::parse_from_rfc3339(value)
                        .map(|time| time.timestamp_millis())
                        .map_err(|error| {
                            anyhow!(
                                "GPX track point {} has invalid RFC3339 time {value:?}: {error}",
                                index + 1
                            )
                        })
                })
                .transpose()?;
            let delay_before_ms = match (previous_timestamp_ms, timestamp_ms) {
                (Some(previous), Some(current)) if current > previous => {
                    u64::try_from(current.saturating_sub(previous)).unwrap_or(u64::MAX)
                }
                _ => 0,
            };
            previous_timestamp_ms = timestamp_ms;
            Ok(LocationSimulationPoint {
                latitude,
                longitude,
                delay_before_ms,
                source_time: point.time,
            })
        })
        .collect()
}

fn parse_coordinate(
    value: &str,
    minimum: f64,
    maximum: f64,
    label: &str,
    index: usize,
) -> anyhow::Result<f64> {
    let parsed = value.parse::<f64>().map_err(|_| {
        anyhow!(
            "GPX track point {} has invalid {label} {value:?}",
            index + 1
        )
    })?;
    if !parsed.is_finite() || !(minimum..=maximum).contains(&parsed) {
        bail!(
            "GPX track point {} {label} must be {minimum}..{maximum}, got {value:?}",
            index + 1
        );
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_namespaced_tracks_and_computes_positive_delays() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("route.gpx");
        std::fs::write(
            &path,
            r#"<?xml version="1.0"?>
<gpx xmlns="http://www.topografix.com/GPX/1/1" version="1.1">
  <trk><name>Demo</name><trkseg>
    <trkpt lat="31.2304" lon="121.4737"><time>2026-07-16T00:00:00Z</time></trkpt>
    <trkpt lat="31.2305" lon="121.4738"><time>2026-07-16T00:00:01.250Z</time></trkpt>
    <trkpt lat="31.2306" lon="121.4739"><time>2026-07-16T00:00:01Z</time></trkpt>
  </trkseg></trk>
</gpx>"#,
        )
        .unwrap();

        let points = parse(&path).unwrap();
        assert_eq!(points.len(), 3);
        assert_eq!(points[0].delay_before_ms, 0);
        assert_eq!(points[1].delay_before_ms, 1_250);
        assert_eq!(points[2].delay_before_ms, 0);
        assert_eq!(points[0].latitude, 31.2304);
        assert_eq!(points[2].longitude, 121.4739);
    }

    #[test]
    fn rejects_missing_points_bad_coordinates_and_bad_times() {
        let directory = tempfile::tempdir().unwrap();
        for (name, contents, expected) in [
            ("empty.gpx", "<gpx><trk /></gpx>", "no track points"),
            (
                "coordinate.gpx",
                "<gpx><trk><trkseg><trkpt lat=\"91\" lon=\"0\" /></trkseg></trk></gpx>",
                "latitude must be",
            ),
            (
                "time.gpx",
                "<gpx><trk><trkseg><trkpt lat=\"1\" lon=\"2\"><time>soon</time></trkpt></trkseg></trk></gpx>",
                "invalid RFC3339 time",
            ),
        ] {
            let path = directory.path().join(name);
            std::fs::write(&path, contents).unwrap();
            assert!(parse(&path).unwrap_err().to_string().contains(expected));
        }
    }
}
