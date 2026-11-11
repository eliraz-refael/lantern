//! Loading images scaled down for display or for the agent.

use std::path::Path;

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, ImageResult};

/// Decodes an image, scales it to fit `max_side` pixels and turns it upright.
pub fn load(path: &Path, max_side: u32) -> ImageResult<DynamicImage> {
    let mut decoder = ImageReader::open(path)?
        .with_guessed_format()?
        .into_decoder()?;
    // Phone photos are often stored sideways with an EXIF flag saying how to rotate them.
    // A malformed EXIF block shouldn't cost the whole image.
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder)?.thumbnail(max_side, max_side);
    image.apply_orientation(orientation);
    Ok(image)
}
