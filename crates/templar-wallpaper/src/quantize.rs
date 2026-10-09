//! Bounded, deterministic WuDP color quantization, emitting R8 indices directly.
//!
//! The algorithm follows the substantive recovered `WUDB.cpp` in
//! `sources/external/wudp-implementation-20261006--2a1cdfbd.md`: weighted LAB PCA,
//! principal-axis projection bins, minimum-SSE contiguous dynamic-programming
//! partitions, LAB centroids, then CIEDE2000 nearest-palette assignment.
//! This is newly authored Rust mathematical reimplementation under the workspace
//! license, not a claim of a license grant for the archived original (which has
//! no explicit license notice), or of the owner's earlier benchmark revision.
//!
//! Deliberate simplifications: an exact-size RGB bucket array replaces elastic
//! hashing and tolerance-based LAB merging; each occupied bucket contributes its
//! frequency and mean encoded-sRGB color. Five bits/channel is the default
//! bounded-quality tradeoff, not an exact-color histogram. No sampling, projection
//! sort (bin order already provides it), expanded RGB output or growing caches are
//! used. Empty bins are removed; duplicate rounded palette colors are removed,
//! never padded. A fixed 3x3 Jacobi eigensolver replaces Eigen. Prefix moments and
//! rolling cost rows replace repeated segment scans and the full DP cost table.
//! Mapping extends the RGB bucketing approximation: each occupied bucket's mean
//! LAB representative is assigned by CIEDE2000 once into a bounded u8 lookup
//! table, then each source pixel emits that bucket's index in O(1).
//!
//! Accepted limits are 1..=256 colors, 1..=1024 projection bins and 1..=6 histogram
//! bits/channel. With H=2^(3*bits), M=bins and K=colors, heap element payload is at
//! most `33*H + (56 + 2*K)*(M+1) + 4*K` bytes (including the returned palette).
//! This is 1,373,752 bytes at defaults and 9,233,976 bytes at maximum options,
//! independent of image size. Fixed stack arrays and eigensolver state add less
//! than 32 KiB; `scratch_bytes` includes that conservative allowance. Allocator
//! metadata is not included. RGB (3N) and caller-owned indices (N) are additional;
//! neither is copied or owned here. The formula conservatively counts histogram,
//! lookup and DP storage together. Mapping costs O(U*K + N), where U<=H is the
//! occupied-bucket count; total CPU complexity is O(N + H + U*K + K*M^2).
//! Quantization remains an explicit worker option.

use std::error::Error;
use std::fmt;

const MAX_BINS: u16 = 1024;
const MAX_HISTOGRAM_BITS: u8 = 6;
const STACK_ALLOWANCE: usize = 32 * 1024;
type Lab = [f64; 3];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuantizeOptions {
    pub colors: u16,
    pub projection_bins: u16,
    pub histogram_bits: u8,
}

impl Default for QuantizeOptions {
    fn default() -> Self {
        Self {
            colors: 256,
            projection_bins: 512,
            histogram_bits: 5,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuantizeError {
    InvalidColors,
    InvalidProjectionBins,
    InvalidHistogramBits,
    EmptyImage,
    SizeOverflow,
    RgbLength { expected: usize, actual: usize },
    IndicesLength { expected: usize, actual: usize },
    AllocationFailed,
}

impl fmt::Display for QuantizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidColors => f.write_str("quantization colors must be in 1..=256"),
            Self::InvalidProjectionBins => f.write_str("projection bins must be in 1..=1024"),
            Self::InvalidHistogramBits => f.write_str("histogram bits must be in 1..=6"),
            Self::EmptyImage => f.write_str("quantization requires nonzero image dimensions"),
            Self::SizeOverflow => f.write_str("quantization image or scratch size overflows"),
            Self::RgbLength { expected, actual } => {
                write!(f, "packed RGB length is {actual}, expected {expected}")
            }
            Self::IndicesLength { expected, actual } => {
                write!(f, "index length is {actual}, expected {expected}")
            }
            Self::AllocationFailed => {
                f.write_str("could not allocate bounded quantization scratch")
            }
        }
    }
}

impl Error for QuantizeError {}

fn validate_options(options: QuantizeOptions) -> Result<(), QuantizeError> {
    if !(1..=256).contains(&options.colors) {
        return Err(QuantizeError::InvalidColors);
    }
    if !(1..=MAX_BINS).contains(&options.projection_bins) {
        return Err(QuantizeError::InvalidProjectionBins);
    }
    if !(1..=MAX_HISTOGRAM_BITS).contains(&options.histogram_bits) {
        return Err(QuantizeError::InvalidHistogramBits);
    }
    Ok(())
}

/// Conservative quantizer scratch plus returned palette, excluding caller buffers
/// and allocator metadata. The decoder can reserve this against its byte budget.
pub fn scratch_bytes(options: QuantizeOptions) -> Result<usize, QuantizeError> {
    validate_options(options)?;
    let histogram = 1_usize << (3 * options.histogram_bits);
    let bins = usize::from(options.projection_bins) + 1;
    let colors = usize::from(options.colors);
    histogram
        .checked_mul(33)
        .and_then(|n| n.checked_add((56 + 2 * colors) * bins))
        .and_then(|n| n.checked_add(4 * colors + STACK_ALLOWANCE))
        .ok_or(QuantizeError::SizeOverflow)
}

#[derive(Clone, Copy, Default)]
struct Bucket {
    count: u64,
    // RGB sums during histogram construction, LAB after conversion in place.
    values: [f64; 3],
}

#[derive(Clone, Copy, Default)]
struct Moment {
    count: u64,
    sum: Lab,
    squared_norm: f64,
}

impl Moment {
    fn add(&mut self, point: Lab, count: u64) {
        self.count += count;
        let weight = count as f64;
        for (sum, value) in self.sum.iter_mut().zip(point) {
            *sum += value * weight;
        }
        self.squared_norm += dot(point, point) * weight;
    }

    fn accumulate(&mut self, other: Self) {
        self.count += other.count;
        for (sum, value) in self.sum.iter_mut().zip(other.sum) {
            *sum += value;
        }
        self.squared_norm += other.squared_norm;
    }
}

fn allocated<T: Clone>(len: usize, value: T) -> Result<Vec<T>, QuantizeError> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(len)
        .map_err(|_| QuantizeError::AllocationFailed)?;
    result.resize(len, value);
    Ok(result)
}

/// Quantize opaque packed sRGB directly into the caller's tightly packed indices.
/// Both lengths must exactly match width*height; errors leave indices untouched.
/// Palette entries are opaque and unique, with deterministic ordering and ties.
pub fn quantize_rgb_into(
    rgb: &[u8],
    width: u32,
    height: u32,
    options: QuantizeOptions,
    indices: &mut [u8],
) -> Result<Vec<[u8; 4]>, QuantizeError> {
    validate_options(options)?;
    if width == 0 || height == 0 {
        return Err(QuantizeError::EmptyImage);
    }
    let pixels = usize::try_from(u64::from(width) * u64::from(height))
        .map_err(|_| QuantizeError::SizeOverflow)?;
    let bytes = pixels.checked_mul(3).ok_or(QuantizeError::SizeOverflow)?;
    if rgb.len() != bytes {
        return Err(QuantizeError::RgbLength {
            expected: bytes,
            actual: rgb.len(),
        });
    }
    if indices.len() != pixels {
        return Err(QuantizeError::IndicesLength {
            expected: pixels,
            actual: indices.len(),
        });
    }

    let mut histogram = allocated(1_usize << (3 * options.histogram_bits), Bucket::default())?;
    let shift = 8 - options.histogram_bits;
    for pixel in rgb.chunks_exact(3) {
        let slot = (usize::from(pixel[0] >> shift) << (2 * options.histogram_bits))
            | (usize::from(pixel[1] >> shift) << options.histogram_bits)
            | usize::from(pixel[2] >> shift);
        let bucket = &mut histogram[slot];
        bucket.count += 1;
        for (sum, channel) in bucket.values.iter_mut().zip(pixel) {
            *sum += f64::from(*channel);
        }
    }
    let occupied = histogram.iter().filter(|bucket| bucket.count != 0).count();
    let mut palette = Vec::new();
    palette
        .try_reserve_exact(usize::from(options.colors))
        .map_err(|_| QuantizeError::AllocationFailed)?;
    let linear = std::array::from_fn::<_, 256, _>(|i| linear_channel(i as f64 / 255.0));

    if occupied <= usize::from(options.colors) {
        // As in recovered WuDP, genuinely small histograms need no partitioning.
        for bucket in histogram.iter_mut().filter(|bucket| bucket.count != 0) {
            let mean = bucket.values.map(|sum| sum / bucket.count as f64);
            let color = mean.map(|value| value.round() as u8);
            push_unique(&mut palette, [color[0], color[1], color[2], 255]);
            bucket.values = linear_rgb_to_lab(mean.map(|value| linear_channel(value / 255.0)));
        }
    } else {
        for bucket in histogram.iter_mut().filter(|bucket| bucket.count != 0) {
            let mean = bucket.values.map(|sum| sum / (255.0 * bucket.count as f64));
            bucket.values = linear_rgb_to_lab(mean.map(linear_channel));
        }
        let (mean, axis) = weighted_pca(&histogram);
        let mut low = f64::INFINITY;
        let mut high = f64::NEG_INFINITY;
        for bucket in histogram.iter().filter(|bucket| bucket.count != 0) {
            let projection = dot(sub(bucket.values, mean), axis);
            low = low.min(projection);
            high = high.max(projection);
        }
        let bin_count = usize::from(options.projection_bins);
        // Leading zero becomes the first prefix; keep it through compaction.
        let mut prefix = allocated(bin_count + 1, Moment::default())?;
        let span = high - low;
        for bucket in histogram.iter().filter(|bucket| bucket.count != 0) {
            let bin = if span <= 1e-12 {
                0
            } else {
                let scaled =
                    ((dot(sub(bucket.values, mean), axis) - low) / span) * bin_count as f64;
                (scaled as usize).min(bin_count - 1)
            };
            prefix[bin + 1].add(bucket.values, bucket.count);
        }
        let mut leading = true;
        prefix.retain(|moment| {
            if leading {
                leading = false;
                true
            } else {
                moment.count != 0
            }
        });
        for i in 1..prefix.len() {
            let previous = prefix[i - 1];
            prefix[i].accumulate(previous);
        }
        let mut centroids = [[0.0; 3]; 256];
        let count = partition(&prefix, usize::from(options.colors), &mut centroids)?;
        for centroid in &centroids[..count] {
            push_unique(&mut palette, lab_to_rgba(*centroid));
        }
    }

    let mapping = nearest_mapping(&histogram, &palette, &linear)?;
    drop(histogram);
    for (pixel, index) in rgb.chunks_exact(3).zip(indices.iter_mut()) {
        let slot = (usize::from(pixel[0] >> shift) << (2 * options.histogram_bits))
            | (usize::from(pixel[1] >> shift) << options.histogram_bits)
            | usize::from(pixel[2] >> shift);
        *index = mapping[slot];
    }
    Ok(palette)
}

fn nearest_mapping(
    histogram: &[Bucket],
    palette: &[[u8; 4]],
    linear: &[f64; 256],
) -> Result<Vec<u8>, QuantizeError> {
    // Measure distance to the actual rounded/clipped returned palette, not the
    // unrepresentable LAB centroids. Each occupied bucket is resolved once.
    let mut palette_lab = [[0.0; 3]; 256];
    for (lab, color) in palette_lab.iter_mut().zip(palette) {
        *lab = rgb_to_lab(&color[..3], linear);
    }
    let mut palette_chroma = [0.0; 256];
    for (chroma, lab) in palette_chroma.iter_mut().zip(&palette_lab[..palette.len()]) {
        *chroma = lab[1].hypot(lab[2]);
    }
    let mut mapping = allocated(histogram.len(), 0_u8)?;
    for (slot, bucket) in histogram
        .iter()
        .enumerate()
        .filter(|(_, bucket)| bucket.count != 0)
    {
        let lab = bucket.values;
        let chroma = lab[1].hypot(lab[2]);
        let mut best = 0;
        let mut distance = f64::INFINITY;
        for (candidate, target) in palette_lab[..palette.len()].iter().enumerate() {
            let current =
                ciede2000_precomputed_squared(lab, chroma, *target, palette_chroma[candidate]);
            if current < distance {
                distance = current;
                best = candidate;
            }
        }
        mapping[slot] = best as u8;
    }
    Ok(mapping)
}

fn push_unique(palette: &mut Vec<[u8; 4]>, color: [u8; 4]) {
    if !palette.contains(&color) {
        palette.push(color);
    }
}

fn sub(a: Lab, b: Lab) -> Lab {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn dot(a: Lab, b: Lab) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn weighted_pca(points: &[Bucket]) -> (Lab, Lab) {
    let total = points.iter().map(|p| p.count).sum::<u64>() as f64;
    let mut mean = [0.0; 3];
    for point in points.iter().filter(|point| point.count != 0) {
        for (sum, value) in mean.iter_mut().zip(point.values) {
            *sum += value * point.count as f64;
        }
    }
    for value in &mut mean {
        *value /= total;
    }
    let mut covariance = [[0.0; 3]; 3];
    for point in points.iter().filter(|point| point.count != 0) {
        let centered = sub(point.values, mean);
        let weight = point.count as f64 / total;
        for i in 0..3 {
            for j in i..3 {
                covariance[i][j] += centered[i] * centered[j] * weight;
            }
        }
    }
    let upper_triangle = covariance;
    for (row_index, row) in covariance.iter_mut().enumerate().skip(1) {
        for (column, value) in row.iter_mut().enumerate().take(row_index) {
            *value = upper_triangle[column][row_index];
        }
    }
    (mean, principal_axis(covariance))
}

fn principal_axis(mut matrix: [[f64; 3]; 3]) -> Lab {
    let mut vectors = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    for _ in 0..32 {
        let (mut p, mut q) = (0, 1);
        for (i, j) in [(0, 2), (1, 2)] {
            if matrix[i][j].abs() > matrix[p][q].abs() {
                p = i;
                q = j;
            }
        }
        let scale = matrix[0][0].abs() + matrix[1][1].abs() + matrix[2][2].abs();
        if matrix[p][q].abs() <= 1e-14 * scale.max(1.0) {
            break;
        }
        let angle = 0.5 * (2.0 * matrix[p][q]).atan2(matrix[q][q] - matrix[p][p]);
        let (sine, cosine) = angle.sin_cos();
        let pp = matrix[p][p];
        let qq = matrix[q][q];
        let pq = matrix[p][q];
        matrix[p][p] = cosine * cosine * pp - 2.0 * sine * cosine * pq + sine * sine * qq;
        matrix[q][q] = sine * sine * pp + 2.0 * sine * cosine * pq + cosine * cosine * qq;
        matrix[p][q] = 0.0;
        matrix[q][p] = 0.0;
        for i in 0..3 {
            if i != p && i != q {
                let ip = matrix[i][p];
                let iq = matrix[i][q];
                matrix[i][p] = cosine * ip - sine * iq;
                matrix[p][i] = matrix[i][p];
                matrix[i][q] = sine * ip + cosine * iq;
                matrix[q][i] = matrix[i][q];
            }
            let vp = vectors[i][p];
            let vq = vectors[i][q];
            vectors[i][p] = cosine * vp - sine * vq;
            vectors[i][q] = sine * vp + cosine * vq;
        }
    }
    let mut largest = 0;
    for i in 1..3 {
        if matrix[i][i] > matrix[largest][largest] {
            largest = i;
        }
    }
    if matrix[largest][largest] <= 1e-12 {
        return [1.0, 0.0, 0.0];
    }
    let mut axis = [
        vectors[0][largest],
        vectors[1][largest],
        vectors[2][largest],
    ];
    let mut sign_component = 0;
    for i in 1..3 {
        if axis[i].abs() > axis[sign_component].abs() {
            sign_component = i;
        }
    }
    if axis[sign_component] < 0.0 {
        for value in &mut axis {
            *value = -*value;
        }
    }
    axis
}

fn segment(prefix: &[Moment], start: usize, end: usize) -> Moment {
    Moment {
        count: prefix[end].count - prefix[start].count,
        sum: sub(prefix[end].sum, prefix[start].sum),
        squared_norm: prefix[end].squared_norm - prefix[start].squared_norm,
    }
}

fn segment_error(prefix: &[Moment], start: usize, end: usize) -> f64 {
    let moment = segment(prefix, start, end);
    (moment.squared_norm - dot(moment.sum, moment.sum) / moment.count as f64).max(0.0)
}

fn centroid(prefix: &[Moment], start: usize, end: usize) -> Lab {
    let moment = segment(prefix, start, end);
    moment.sum.map(|value| value / moment.count as f64)
}

fn partition(
    prefix: &[Moment],
    requested: usize,
    palette: &mut [Lab; 256],
) -> Result<usize, QuantizeError> {
    let bins = prefix.len() - 1;
    let colors = requested.min(bins);
    if colors == bins {
        for (i, point) in palette[..colors].iter_mut().enumerate() {
            *point = centroid(prefix, i, i + 1);
        }
        return Ok(colors);
    }
    if colors == 1 {
        palette[0] = centroid(prefix, 0, bins);
        return Ok(1);
    }
    let stride = bins + 1;
    let mut previous = allocated(stride, f64::INFINITY)?;
    let mut current = allocated(stride, f64::INFINITY)?;
    let mut cuts = allocated(colors * stride, 0_u16)?;
    for (end, cost) in previous.iter_mut().enumerate().skip(1) {
        *cost = segment_error(prefix, 0, end);
    }
    for k in 2..=colors {
        current.fill(f64::INFINITY);
        for end in k..=bins {
            // Ascending cuts and strict comparison give deterministic first ties.
            for (cut, previous_cost) in previous.iter().enumerate().take(end).skip(k - 1) {
                let cost = previous_cost + segment_error(prefix, cut, end);
                if cost < current[end] {
                    current[end] = cost;
                    cuts[(k - 1) * stride + end] = cut as u16;
                }
            }
        }
        std::mem::swap(&mut previous, &mut current);
    }
    let mut end = bins;
    for k in (1..=colors).rev() {
        let start = if k == 1 {
            0
        } else {
            usize::from(cuts[(k - 1) * stride + end])
        };
        palette[k - 1] = centroid(prefix, start, end);
        end = start;
    }
    Ok(colors)
}

fn linear_channel(value: f64) -> f64 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn encoded_channel(value: f64) -> u8 {
    let value = value.clamp(0.0, 1.0);
    let encoded = if value <= 0.003_130_8 {
        12.92 * value
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    };
    (255.0 * encoded).round().clamp(0.0, 255.0) as u8
}

fn rgb_to_lab(rgb: &[u8], linear: &[f64; 256]) -> Lab {
    linear_rgb_to_lab([
        linear[usize::from(rgb[0])],
        linear[usize::from(rgb[1])],
        linear[usize::from(rgb[2])],
    ])
}

fn lab_curve(value: f64) -> f64 {
    if value > 216.0 / 24389.0 {
        value.cbrt()
    } else {
        (24389.0 / 27.0 * value + 16.0) / 116.0
    }
}

fn linear_rgb_to_lab(rgb: Lab) -> Lab {
    // IEC sRGB / CIE XYZ, D65 2-degree reference white (no chromatic adaptation).
    let x =
        lab_curve((0.412_456_4 * rgb[0] + 0.357_576_1 * rgb[1] + 0.180_437_5 * rgb[2]) / 0.950_47);
    let y = lab_curve(0.212_672_9 * rgb[0] + 0.715_152_2 * rgb[1] + 0.072_175_0 * rgb[2]);
    let z =
        lab_curve((0.019_333_9 * rgb[0] + 0.119_192_0 * rgb[1] + 0.950_304_1 * rgb[2]) / 1.088_83);
    [116.0 * y - 16.0, 500.0 * (x - y), 200.0 * (y - z)]
}

fn inverse_lab_curve(value: f64) -> f64 {
    if value > 6.0 / 29.0 {
        value * value * value
    } else {
        3.0 * (6.0 / 29.0_f64).powi(2) * (value - 4.0 / 29.0)
    }
}

fn lab_to_rgba(lab: Lab) -> [u8; 4] {
    let fy = (lab[0] + 16.0) / 116.0;
    let x = 0.950_47 * inverse_lab_curve(fy + lab[1] / 500.0);
    let y = inverse_lab_curve(fy);
    let z = 1.088_83 * inverse_lab_curve(fy - lab[2] / 200.0);
    [
        encoded_channel(3.240_454_2 * x - 1.537_138_5 * y - 0.498_531_4 * z),
        encoded_channel(-0.969_266_0 * x + 1.876_010_8 * y + 0.041_556_0 * z),
        encoded_channel(0.055_643_4 * x - 0.204_025_9 * y + 1.057_225_2 * z),
        255,
    ]
}

fn seventh_power(value: f64) -> f64 {
    let square = value * value;
    square * square * square * value
}

#[cfg(test)]
fn ciede2000_squared(first: Lab, second: Lab) -> f64 {
    ciede2000_precomputed_squared(
        first,
        first[1].hypot(first[2]),
        second,
        second[1].hypot(second[2]),
    )
}

/// CIEDE2000 with unit lightness/chroma/hue weights. Squared distance suffices
/// for nearest mapping and avoids one square root for every candidate.
fn ciede2000_precomputed_squared(first: Lab, c1: f64, second: Lab, c2: f64) -> f64 {
    use std::f64::consts::{PI, TAU};
    const POW_25_7: f64 = 6_103_515_625.0;
    let chroma_power = seventh_power((c1 + c2) * 0.5);
    let chroma_correction = 0.5 * (1.0 - (chroma_power / (chroma_power + POW_25_7)).sqrt());
    let a1 = (1.0 + chroma_correction) * first[1];
    let a2 = (1.0 + chroma_correction) * second[1];
    let cp1 = a1.hypot(first[2]);
    let cp2 = a2.hypot(second[2]);
    let hue = |a: f64, b: f64| {
        if a == 0.0 && b == 0.0 {
            0.0
        } else {
            b.atan2(a).rem_euclid(TAU)
        }
    };
    let h1 = hue(a1, first[2]);
    let h2 = hue(a2, second[2]);
    let achromatic = cp1 == 0.0 || cp2 == 0.0;
    let mut dh = h2 - h1;
    if achromatic {
        dh = 0.0;
    } else if dh > PI {
        dh -= TAU;
    } else if dh < -PI {
        dh += TAU;
    }
    let dl = second[0] - first[0];
    let dc = cp2 - cp1;
    let dh_scaled = 2.0 * (cp1 * cp2).sqrt() * (dh * 0.5).sin();
    let mean_l = (first[0] + second[0]) * 0.5;
    let mean_c = (cp1 + cp2) * 0.5;
    let mean_h = if achromatic {
        h1 + h2
    } else if (h1 - h2).abs() <= PI {
        (h1 + h2) * 0.5
    } else if h1 + h2 < TAU {
        (h1 + h2 + TAU) * 0.5
    } else {
        (h1 + h2 - TAU) * 0.5
    };
    let radians = PI / 180.0;
    let hue_weight = 1.0 - 0.17 * (mean_h - 30.0 * radians).cos()
        + 0.24 * (2.0 * mean_h).cos()
        + 0.32 * (3.0 * mean_h + 6.0 * radians).cos()
        - 0.20 * (4.0 * mean_h - 63.0 * radians).cos();
    let offset_l = (mean_l - 50.0).powi(2);
    let sl = 1.0 + 0.015 * offset_l / (20.0 + offset_l).sqrt();
    let sc = 1.0 + 0.045 * mean_c;
    let sh = 1.0 + 0.015 * mean_c * hue_weight;
    let corrected_power = seventh_power(mean_c);
    let rc = 2.0 * (corrected_power / (corrected_power + POW_25_7)).sqrt();
    let delta_theta = 30.0 * radians * (-((mean_h / radians - 275.0) / 25.0).powi(2)).exp();
    let rt = -(2.0 * delta_theta).sin() * rc;
    let lightness = dl / sl;
    let chroma = dc / sc;
    let hue_distance = dh_scaled / sh;
    (lightness * lightness
        + chroma * chroma
        + hue_distance * hue_distance
        + rt * chroma * hue_distance)
        .max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quantize(pixels: &[[u8; 3]], options: QuantizeOptions) -> (Vec<[u8; 4]>, Vec<u8>) {
        let rgb: Vec<u8> = pixels.iter().flatten().copied().collect();
        let mut indices = vec![255; pixels.len()];
        let palette =
            quantize_rgb_into(&rgb, pixels.len() as u32, 1, options, &mut indices).unwrap();
        (palette, indices)
    }

    #[test]
    fn solid_and_small_images_have_exact_opaque_colors_without_padding() {
        let (palette, indices) = quantize(&[[23, 91, 147]; 7], QuantizeOptions::default());
        assert_eq!(palette, [[23, 91, 147, 255]]);
        assert_eq!(indices, [0; 7]);
        let pixels = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 0, 0]];
        let (palette, indices) = quantize(&pixels, QuantizeOptions::default());
        assert_eq!(palette.len(), 3);
        for (pixel, index) in pixels.iter().zip(indices) {
            assert_eq!(&palette[usize::from(index)][..3], pixel);
        }
    }

    #[test]
    fn single_color_is_frequency_weighted_lab_not_rgb_average() {
        let options = QuantizeOptions {
            colors: 1,
            ..QuantizeOptions::default()
        };
        let (palette, indices) =
            quantize(&[[0, 0, 0], [0, 0, 0], [0, 0, 0], [255, 255, 255]], options);
        assert_eq!(palette, [[59, 59, 59, 255]]);
        assert_eq!(indices, [0; 4]);
    }

    #[test]
    fn dp_separates_distant_clusters_and_is_order_independent() {
        let pixels = [[0; 3], [8; 3], [16; 3], [224; 3], [232; 3], [240; 3]];
        let options = QuantizeOptions {
            colors: 2,
            histogram_bits: 6,
            ..QuantizeOptions::default()
        };
        let (palette, indices) = quantize(&pixels, options);
        assert_eq!(palette.len(), 2);
        assert!(palette[0][0] < 32 && palette[1][0] > 208);
        assert_eq!(indices, [0, 0, 0, 1, 1, 1]);
        let reversed: Vec<_> = pixels.into_iter().rev().collect();
        let (other_palette, other_indices) = quantize(&reversed, options);
        assert_eq!(palette, other_palette);
        assert_eq!(other_indices, [1, 1, 1, 0, 0, 0]);
    }

    #[test]
    fn full_256_color_consumer_output_is_deterministic_unique_and_indexed() {
        let pixels: Vec<_> = (0..32_u8)
            .flat_map(|red| {
                (0..16_u8).map(move |green| [red * 8, green * 16, red.wrapping_mul(73)])
            })
            .collect();
        let (palette, indices) = quantize(&pixels, QuantizeOptions::default());
        assert!(!palette.is_empty() && palette.len() <= 256);
        assert_eq!(indices.len(), pixels.len());
        assert!(
            indices
                .iter()
                .all(|index| usize::from(*index) < palette.len())
        );
        assert!(palette.iter().all(|color| color[3] == 255));
        for (i, color) in palette.iter().enumerate() {
            assert!(!palette[..i].contains(color));
        }
        let (again_palette, again_indices) = quantize(&pixels, QuantizeOptions::default());
        assert_eq!(palette, again_palette);
        assert_eq!(indices, again_indices);
        assert_ne!(indices[0], indices[indices.len() - 1]);
        // Principal-axis bins can merge distant perpendicular LAB components;
        // WuDP does not promise per-channel error bounds or preserved endpoints.
        // What it does promise is traversal-independent training and assignment.
        let reversed: Vec<_> = pixels.iter().rev().copied().collect();
        let (reversed_palette, reversed_indices) = quantize(&reversed, QuantizeOptions::default());
        assert_eq!(palette, reversed_palette);
        assert_eq!(
            indices.into_iter().rev().collect::<Vec<_>>(),
            reversed_indices
        );
    }

    #[test]
    #[allow(
        clippy::float_cmp,
        reason = "pairwise integer centroids must be exactly half integers"
    )]
    fn default_256_colors_runs_partition_when_more_than_256_bins_are_occupied() {
        let mut prefix = vec![Moment::default(); 513];
        for i in 1..=512 {
            prefix[i].add([i as f64, 0.0, 0.0], 1);
            let previous = prefix[i - 1];
            prefix[i].accumulate(previous);
        }
        let mut palette = [[0.0; 3]; 256];
        assert_eq!(partition(&prefix, 256, &mut palette).unwrap(), 256);
        for (i, lab) in palette.iter().enumerate() {
            assert_eq!(*lab, [2.0 * i as f64 + 1.5, 0.0, 0.0]);
        }
    }

    #[test]
    fn empty_projection_bins_and_one_bin_never_create_dummy_colors() {
        let options = QuantizeOptions {
            colors: 2,
            projection_bins: 1,
            ..QuantizeOptions::default()
        };
        let (palette, indices) = quantize(&[[0; 3], [128; 3], [255; 3]], options);
        assert_eq!(palette.len(), 1);
        assert_eq!(indices, [0; 3]);
        let options = QuantizeOptions {
            colors: 2,
            projection_bins: 1024,
            ..QuantizeOptions::default()
        };
        let (palette, indices) = quantize(&[[0; 3], [8; 3], [255; 3]], options);
        assert_eq!(palette.len(), 2);
        assert_eq!(indices, [0, 0, 1]);
    }

    #[test]
    fn duplicate_centroid_colors_are_removed_and_ties_keep_first() {
        let mut palette = Vec::with_capacity(3);
        push_unique(&mut palette, lab_to_rgba([50.0, 0.0, 0.0]));
        push_unique(&mut palette, lab_to_rgba([50.000_001, 0.0, 0.0]));
        assert_eq!(palette.len(), 1);
        let (palette, indices) = quantize(
            &[[0; 3], [255; 3]],
            QuantizeOptions {
                colors: 1,
                ..QuantizeOptions::default()
            },
        );
        assert_eq!(palette.len(), 1);
        assert_eq!(indices, [0, 0]);
    }

    #[test]
    #[allow(
        clippy::float_cmp,
        reason = "zero and diagonal matrices have exact coordinate axes"
    )]
    fn pca_handles_degeneracy_and_axis_not_reachable_from_lightness_seed() {
        assert_eq!(principal_axis([[0.0; 3]; 3]), [1.0, 0.0, 0.0]);
        assert_eq!(
            principal_axis([[0.0, 0.0, 0.0], [0.0, 9.0, 0.0], [0.0, 0.0, 1.0]]),
            [0.0, 1.0, 0.0]
        );
        let axis = principal_axis([[1.0, 2.0, 0.0], [2.0, 4.0, 0.0], [0.0, 0.0, 0.0]]);
        assert!((axis[0] - 1.0 / 5.0_f64.sqrt()).abs() < 1e-12);
        assert!((axis[1] - 2.0 / 5.0_f64.sqrt()).abs() < 1e-12);
    }

    #[test]
    fn ciede2000_matches_published_sharma_reference_pairs() {
        // Sharma, Wu, Dalal (2005), supplementary CIEDE2000 test data.
        for (first, second, expected) in [
            ([50.0, 2.6772, -79.7751], [50.0, 0.0, -82.7485], 2.0425),
            ([50.0, 3.1571, -77.2803], [50.0, 0.0, -82.7485], 2.8615),
            ([50.0, 0.0, 0.0], [50.0, -1.0, 2.0], 2.3669),
            ([50.0, 2.49, -0.001], [50.0, -2.49, 0.0009], 7.1792),
        ] {
            assert!((ciede2000_squared(first, second).sqrt() - expected).abs() < 0.0001);
            assert!((ciede2000_squared(second, first).sqrt() - expected).abs() < 0.0001);
        }
    }

    #[test]
    fn rgb_lab_roundtrip_preserves_primaries_and_grays() {
        let linear = std::array::from_fn(|i| linear_channel(i as f64 / 255.0));
        for rgb in [
            [0, 0, 0],
            [255, 255, 255],
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [17, 125, 203],
        ] {
            assert_eq!(
                lab_to_rgba(rgb_to_lab(&rgb, &linear)),
                [rgb[0], rgb[1], rgb[2], 255]
            );
        }
    }

    #[test]
    fn invalid_inputs_preserve_indices_and_reject_overflow() {
        let mut output = [42];
        let valid = QuantizeOptions::default();
        assert_eq!(
            quantize_rgb_into(&[], 0, 1, valid, &mut output),
            Err(QuantizeError::EmptyImage)
        );
        assert!(matches!(
            quantize_rgb_into(&[], u32::MAX, u32::MAX, valid, &mut output),
            Err(QuantizeError::SizeOverflow)
        ));
        assert_eq!(
            quantize_rgb_into(&[0, 0], 1, 1, valid, &mut output),
            Err(QuantizeError::RgbLength {
                expected: 3,
                actual: 2
            })
        );
        assert_eq!(
            quantize_rgb_into(&[0; 6], 2, 1, valid, &mut output),
            Err(QuantizeError::IndicesLength {
                expected: 2,
                actual: 1
            })
        );
        for colors in [0, 257] {
            assert_eq!(
                quantize_rgb_into(
                    &[0; 3],
                    1,
                    1,
                    QuantizeOptions { colors, ..valid },
                    &mut output
                ),
                Err(QuantizeError::InvalidColors)
            );
        }
        for projection_bins in [0, 1025] {
            assert_eq!(
                quantize_rgb_into(
                    &[0; 3],
                    1,
                    1,
                    QuantizeOptions {
                        projection_bins,
                        ..valid
                    },
                    &mut output
                ),
                Err(QuantizeError::InvalidProjectionBins)
            );
        }
        for histogram_bits in [0, 7, 255] {
            assert_eq!(
                quantize_rgb_into(
                    &[0; 3],
                    1,
                    1,
                    QuantizeOptions {
                        histogram_bits,
                        ..valid
                    },
                    &mut output
                ),
                Err(QuantizeError::InvalidHistogramBits)
            );
        }
        assert_eq!(output, [42]);
    }
}
