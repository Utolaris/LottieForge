//! Pure per-frame pixel operations.
//!
//! Deliberately free of any I/O or rlottie dependency so the transforms can be
//! exercised by unit tests on their own.

/// The mirroring and rotation applied to every rendered frame.
#[derive(Clone, Copy, Debug)]
pub struct FrameTransform {
    pub rotation_degrees: f64,
    pub flip_horizontal: bool,
    pub flip_vertical: bool,
}

impl FrameTransform {
    /// Applies the flips and the rotation, then un-premultiplies alpha.
    ///
    /// Both steps have to happen while the pixels are still pre-multiplied:
    /// interpolating straight (non-premultiplied) RGBA would darken the
    /// transparent edges of every rotated frame.
    pub fn apply(&self, mut rgba: Vec<u8>, width: usize, height: usize) -> Vec<u8> {
        if self.flip_horizontal || self.flip_vertical {
            rgba = flip_rgba(
                &rgba,
                width,
                height,
                self.flip_horizontal,
                self.flip_vertical,
            );
        }

        let angle = self.rotation_degrees.rem_euclid(360.0);
        if angle.abs() > f64::EPSILON {
            rgba = rotate_premultiplied_rgba(&rgba, width, height, angle.to_radians());
        }

        unpremultiply_rgba(&mut rgba);
        rgba
    }
}

fn flip_rgba(
    source: &[u8],
    width: usize,
    height: usize,
    horizontal: bool,
    vertical: bool,
) -> Vec<u8> {
    let mut output = vec![0; source.len()];
    for y in 0..height {
        for x in 0..width {
            let source_x = if horizontal { width - 1 - x } else { x };
            let source_y = if vertical { height - 1 - y } else { y };
            let source_offset = (source_y * width + source_x) * 4;
            let destination_offset = (y * width + x) * 4;
            output[destination_offset..destination_offset + 4]
                .copy_from_slice(&source[source_offset..source_offset + 4]);
        }
    }
    output
}

fn rotate_premultiplied_rgba(source: &[u8], width: usize, height: usize, radians: f64) -> Vec<u8> {
    let mut output = vec![0; source.len()];
    let cosine = radians.cos();
    let sine = radians.sin();
    let center_x = (width as f64 - 1.0) / 2.0;
    let center_y = (height as f64 - 1.0) / 2.0;

    for y in 0..height {
        for x in 0..width {
            let dx = x as f64 - center_x;
            let dy = y as f64 - center_y;
            let source_x = cosine * dx + sine * dy + center_x;
            let source_y = -sine * dx + cosine * dy + center_y;
            let pixel = bilinear_sample(source, width, height, source_x, source_y);
            let destination_offset = (y * width + x) * 4;
            output[destination_offset..destination_offset + 4].copy_from_slice(&pixel);
        }
    }
    output
}

fn bilinear_sample(source: &[u8], width: usize, height: usize, x: f64, y: f64) -> [u8; 4] {
    let left = x.floor() as isize;
    let top = y.floor() as isize;
    let fraction_x = x - left as f64;
    let fraction_y = y - top as f64;
    let samples = [
        sample_pixel(source, width, height, left, top),
        sample_pixel(source, width, height, left + 1, top),
        sample_pixel(source, width, height, left, top + 1),
        sample_pixel(source, width, height, left + 1, top + 1),
    ];
    let weights = [
        (1.0 - fraction_x) * (1.0 - fraction_y),
        fraction_x * (1.0 - fraction_y),
        (1.0 - fraction_x) * fraction_y,
        fraction_x * fraction_y,
    ];

    let mut output = [0; 4];
    for channel in 0..4 {
        let value = samples
            .iter()
            .zip(weights)
            .map(|(sample, weight)| f64::from(sample[channel]) * weight)
            .sum::<f64>();
        output[channel] = value.round().clamp(0.0, 255.0) as u8;
    }
    output
}

fn sample_pixel(source: &[u8], width: usize, height: usize, x: isize, y: isize) -> [u8; 4] {
    if x < 0 || y < 0 || x >= width as isize || y >= height as isize {
        return [0; 4];
    }
    let offset = (y as usize * width + x as usize) * 4;
    [
        source[offset],
        source[offset + 1],
        source[offset + 2],
        source[offset + 3],
    ]
}

fn unpremultiply_rgba(rgba: &mut [u8]) {
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        if alpha == 0 {
            pixel[..3].fill(0);
            continue;
        }
        for channel in &mut pixel[..3] {
            let expanded = (u16::from(*channel) * 255 + alpha / 2) / alpha;
            *channel = expanded.min(255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FrameTransform;

    #[test]
    fn horizontal_flip_reorders_pixels() {
        let input = vec![1, 0, 0, 255, 2, 0, 0, 255, 3, 0, 0, 255, 4, 0, 0, 255];
        let transform = FrameTransform {
            rotation_degrees: 0.0,
            flip_horizontal: true,
            flip_vertical: false,
        };
        let output = transform.apply(input, 2, 2);
        assert_eq!(output[0], 2);
        assert_eq!(output[4], 1);
        assert_eq!(output[8], 4);
        assert_eq!(output[12], 3);
    }

    #[test]
    fn transparent_pixels_remain_transparent_after_transform() {
        let transform = FrameTransform {
            rotation_degrees: 45.0,
            flip_horizontal: true,
            flip_vertical: true,
        };
        let output = transform.apply(vec![0; 16], 2, 2);
        assert!(output.chunks_exact(4).all(|pixel| pixel[3] == 0));
    }

    #[test]
    fn a_full_turn_is_a_no_op() {
        let transform = FrameTransform {
            rotation_degrees: 360.0,
            flip_horizontal: false,
            flip_vertical: false,
        };
        let input = vec![
            10, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, 1, 2, 3, 255,
        ];
        let expected = input.clone();
        assert_eq!(transform.apply(input, 2, 2), expected);
    }
}
