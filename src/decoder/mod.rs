use std::error::Error;
use std::io::{self, Read, Seek};
use std::fmt;
use std::mem;
use num_traits::{FromPrimitive, Num};
use std::collections::HashMap;

use self::ifd::Directory;

use self::stream::{
    ByteOrder,
    EndianReader,
    SmartReader,
    LZWReader,
    PackBitsReader
};

mod ifd;
mod stream;

/// An enumeration over supported color types and their bit depths
#[derive(Copy, PartialEq, Eq, Debug, Clone, Hash)]
pub enum ColorType {
    /// Pixel is grayscale
    Gray(u8),

    /// Pixel contains R, G and B channels
    RGB(u8),

    /// Pixel is an index into a color palette
    Palette(u8),

    /// Pixel is grayscale with an alpha channel
    GrayA(u8),

    /// Pixel is RGB with an alpha channel
    RGBA(u8),
}

/// An enumeration of Image errors
#[derive(Debug)]
pub enum ImageError {
    /// The Image is not formatted properly
    FormatError(String),

    /// The Image's dimensions are either too small or too large
    DimensionError,

    /// The Decoder does not support this image format
    UnsupportedError(String),

    /// The Decoder does not support this color type
    UnsupportedColor(ColorType),

    /// Not enough data was provided to the Decoder
    /// to decode the image
    NotEnoughData,

    /// An I/O Error occurred while decoding the image
    IoError(io::Error),

    /// The end of the image has been reached
    ImageEnd
}

impl fmt::Display for ImageError {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match *self {
            ImageError::FormatError(ref e) => write!(fmt, "Format error: {}", e),
            ImageError::DimensionError => write!(fmt, "The Image's dimensions are either too \
                                                        small or too large"),
            ImageError::UnsupportedError(ref f) => write!(fmt, "The Decoder does not support the \
                                                                 image format `{}`", f),
            ImageError::UnsupportedColor(ref c) => write!(fmt, "The decoder does not support \
                                                                 the color type `{:?}`", c),
            ImageError::NotEnoughData => write!(fmt, "Not enough data was provided to the \
                                                       Decoder to decode the image"),
            ImageError::IoError(ref e) => e.fmt(fmt),
            ImageError::ImageEnd => write!(fmt, "The end of the image has been reached")
        }
    }
}

impl Error for ImageError {
    fn description (&self) -> &str {
        match *self {
            ImageError::FormatError(..) => "Format error",
            ImageError::DimensionError => "Dimension error",
            ImageError::UnsupportedError(..) => "Unsupported error",
            ImageError::UnsupportedColor(..) => "Unsupported color",
            ImageError::NotEnoughData => "Not enough data",
            ImageError::IoError(..) => "IO error",
            ImageError::ImageEnd => "Image end"
        }
    }

    fn cause (&self) -> Option<&Error> {
        match *self {
            ImageError::IoError(ref e) => Some(e),
            _ => None
        }
    }
}

impl From<io::Error> for ImageError {
    fn from(err: io::Error) -> ImageError {
        ImageError::IoError(err)
    }
}

/// Result of an image decoding/encoding process
pub type ImageResult<T> = Result<T, ImageError>;

/// Result of a decoding process
#[derive(Debug)]
pub enum DecodingResult {
    /// A vector of unsigned bytes
    U8(Vec<u8>),
    /// A vector of unsigned words
    U16(Vec<u16>)
}

// A buffer for image decoding
pub enum DecodingBuffer<'a> {
    /// A slice of unsigned bytes
    U8(&'a mut [u8]),
    /// A slice of unsigned words
    U16(&'a mut [u16])
}

enum_from_primitive! {
#[derive(Clone, Copy, Debug, PartialEq)]
enum PhotometricInterpretation {
    WhiteIsZero = 0,
    BlackIsZero = 1,
    RGB = 2,
    RGBPalette = 3,
    TransparencyMask = 4,
    CMYK = 5,
    YCbCr = 6,
    CIELab = 8,
}
}

enum_from_primitive! {
#[derive(Clone, Copy, Debug)]
enum CompressionMethod {
    None = 1,
    Huffman = 2,
    Fax3 = 3,
    Fax4 = 4,
    LZW = 5,
    JPEG = 6,
    PackBits = 0x8005
}
}

enum_from_primitive! {
#[derive(Clone, Copy, Debug)]
enum PlanarConfiguration {
    Chunky = 1,
    Planar = 2
}
}

enum_from_primitive! {
#[derive(Clone, Copy, Debug)]
enum Predictor {
    None = 1,
    Horizontal = 2
}
}

/// The representation of a TIFF decoder
///
/// Currently does not support decoding of interlaced images
#[derive(Debug)]
pub struct TIFFDecoder<R> where R: Read + Seek {
    reader: SmartReader<R>,
    byte_order: ByteOrder,
    next_ifd: Option<u32>,
    ifd: Option<Directory>,
    width: u32,
    height: u32,
    bits_per_sample: Vec<u8>,
    samples: u8,
    photometric_interpretation: PhotometricInterpretation,
    compression_method: CompressionMethod
}

trait Wrapping {
    fn wrapping_add(&self, other: Self) -> Self;
}

impl Wrapping for u8 {
    fn wrapping_add(&self, other: Self) -> Self {
        u8::wrapping_add(*self, other)
    }
}

impl Wrapping for u16 {
    fn wrapping_add(&self, other: Self) -> Self {
        u16::wrapping_add(*self, other)
    }
}

fn rev_hpredict_nsamp<T>(mut image: Vec<T>,
                         size: (u32, u32),
                         samples: usize)
                         -> Vec<T>
                         where T: Num + Copy + Wrapping {
    let width = size.0 as usize;
    let height = size.1 as usize;
    for row in 0..height {
        for col in samples..width * samples {
            let prev_pixel = image[(row * width * samples + col - samples)];
            let pixel = &mut image[(row * width * samples + col)];
            *pixel = pixel.wrapping_add(prev_pixel);
        }
    }
    image
}

fn rev_hpredict(image: DecodingResult, size: (u32, u32), color_type: ColorType) -> ImageResult<DecodingResult> {
    let samples = match color_type {
        ColorType::Gray(8) | ColorType::Gray(16) => 1,
        ColorType::RGB(8) | ColorType::RGB(16) => 3,
        ColorType::RGBA(8) | ColorType::RGBA(16) => 4,
        _ => return Err(ImageError::UnsupportedError(format!(
            "Horizontal predictor for {:?} is unsupported.", color_type
        )))
    };
    Ok(match image {
        DecodingResult::U8(buf) => {
            DecodingResult::U8(rev_hpredict_nsamp(buf, size, samples))
        },
        DecodingResult::U16(buf) => {
            DecodingResult::U16(rev_hpredict_nsamp(buf, size, samples))
        }
    })
}

impl<R: Read + Seek> TIFFDecoder<R> {
    /// Create a new decoder that decodes from the stream ```r```
    pub fn new(r: R) -> ImageResult<TIFFDecoder<R>> {
        TIFFDecoder {
            reader: SmartReader::wrap(r, ByteOrder::LittleEndian),
            byte_order: ByteOrder::LittleEndian,
            next_ifd: None,
            ifd: None,
            width: 0,
            height: 0,
            bits_per_sample: vec![1],
            samples: 1,
            photometric_interpretation: PhotometricInterpretation::BlackIsZero,
            compression_method: CompressionMethod::None
        }.init()
    }

    fn colortype(&mut self) -> ImageResult<ColorType> {
        match self.photometric_interpretation {
            // TODO: catch also [ 8, 8, 8, _] this does not work due to a bug in rust atm
            PhotometricInterpretation::RGB if self.bits_per_sample == [8, 8, 8, 8] => Ok(ColorType::RGBA(8)),
            PhotometricInterpretation::RGB if self.bits_per_sample == [8, 8, 8] => Ok(ColorType::RGB(8)),
            PhotometricInterpretation::RGB if self.bits_per_sample == [16, 16, 16, 16] => Ok(ColorType::RGBA(16)),
            PhotometricInterpretation::RGB if self.bits_per_sample == [16, 16, 16] => Ok(ColorType::RGB(16)),
            PhotometricInterpretation::BlackIsZero | PhotometricInterpretation::WhiteIsZero
                                           if self.bits_per_sample.len() == 1 => Ok(ColorType::Gray(self.bits_per_sample[0])),

            _ => Err(ImageError::UnsupportedError(format!(
                "{:?} with {:?} bits per sample is unsupported", self.bits_per_sample, self.photometric_interpretation
            ))) // TODO: this is bad we should not fail at this point}
        }
    }

    fn read_header(&mut self) -> ImageResult<()> {
        let mut endianess = Vec::with_capacity(2);
        try!(self.reader.by_ref().take(2).read_to_end(&mut endianess));
        match &*endianess {
            b"II" => {
                self.byte_order = ByteOrder::LittleEndian;
                self.reader.byte_order = ByteOrder::LittleEndian; },
            b"MM" => {
                self.byte_order = ByteOrder::BigEndian;
                self.reader.byte_order = ByteOrder::BigEndian;  },
            _ => return Err(ImageError::FormatError(
                "TIFF signature not found.".to_string()
            ))
        }
        if try!(self.read_short()) != 42 {
            return Err(ImageError::FormatError("TIFF signature invalid.".to_string()))
        }
        self.next_ifd = match try!(self.read_long()) {
            0 => None,
            n => Some(n)
        };
        Ok(())
    }

    /// Initializes the decoder.
    pub fn init(mut self) -> ImageResult<TIFFDecoder<R>> {
        try!(self.read_header());
        self.next_image()
    }

    /// Reads in the next image.
    /// If there is no further image in the TIFF file a format error is returned.
    /// To determine whether there are more images call `TIFFDecoder::more_images` instead.
    pub fn next_image(mut self) -> ImageResult<TIFFDecoder<R>> {
        self.ifd = Some(try!(self.read_ifd()));
        self.width = try!(self.get_tag_u32(ifd::Tag::ImageWidth));
        self.height = try!(self.get_tag_u32(ifd::Tag::ImageLength));
        self.photometric_interpretation = match FromPrimitive::from_u32(
            try!(self.get_tag_u32(ifd::Tag::PhotometricInterpretation))
        ) {
            Some(val) => val,
            None => return Err(ImageError::UnsupportedError(
                "The image is using an unknown photometric interpretation.".to_string()
            ))
        };
        if let Some(val) = try!(self.find_tag_u32(ifd::Tag::Compression)) {
            match FromPrimitive::from_u32(val) {
                Some(method) =>  {
                    self.compression_method = method
                },
                None => return Err(ImageError::UnsupportedError(
                    "Unknown compression method.".to_string()
                ))
            }
        }
        if let Some(val) = try!(self.find_tag_u32(ifd::Tag::SamplesPerPixel)) {
            self.samples = val as u8
        }
        match self.samples {
            1 => {
                if let Some(val) = try!(self.find_tag_u32(ifd::Tag::BitsPerSample)) {
                    self.bits_per_sample = vec![val as u8]
                }
            }
            3 | 4 => {
                if let Some(val) = try!(self.find_tag_u32_vec(ifd::Tag::BitsPerSample)) {
                    self.bits_per_sample = val.iter().map(|&v| v as u8).collect()
                }

            }
            _ => return Err(ImageError::UnsupportedError(
                format!("{} samples per pixel is supported.", self.samples)
            ))
        }
        Ok(self)
    }

    /// Returns `true` if there is at least one more image available.
    pub fn more_images(&self) -> bool {
        match self.next_ifd {
            Some(_) => true,
            None => false
        }
    }

    /// Returns the byte_order
    pub fn byte_order(&self) -> ByteOrder {
        self.byte_order
    }

    /// Reads a TIFF short value
    #[inline]
    pub fn read_short(&mut self) -> Result<u16, io::Error> {
        self.reader.read_u16()
    }

    /// Reads a TIFF long value
    #[inline]
    pub fn read_long(&mut self) -> Result<u32, io::Error> {
        self.reader.read_u32()
    }

    /// Reads a TIFF IFA offset/value field
    #[inline]
    pub fn read_offset(&mut self) -> Result<[u8; 4], io::Error> {
        let mut val = [0; 4];
        try!(self.reader.read_exact(&mut val));
        Ok(val)
    }

    /// Moves the cursor to the specified offset
    #[inline]
    pub fn goto_offset(&mut self, offset: u32) -> io::Result<()> {
        self.reader.seek(io::SeekFrom::Start(u64::from(offset))).map(|_| ())
    }

    /// Reads a IFD entry.
    // An IFD entry has four fields:
    //
    // Tag   2 bytes
    // Type  2 bytes
    // Count 4 bytes
    // Value 4 bytes either a pointer the value itself
    fn read_entry(&mut self) -> ImageResult<Option<(ifd::Tag, ifd::Entry)>> {
        let tag = ifd::Tag::from_u16(try!(self.read_short()));
        let type_: ifd::Type = match FromPrimitive::from_u16(try!(self.read_short())) {
            Some(t) => t,
            None => {
                // Unknown type. Skip this entry according to spec.
                try!(self.read_long());
                try!(self.read_long());
                return Ok(None)

            }
        };
        Ok(Some((tag, ifd::Entry::new(
            type_,
            try!(self.read_long()), // count
            try!(self.read_offset())  // offset
        ))))
    }

    /// Reads the next IFD
    fn read_ifd(&mut self) -> ImageResult<Directory> {
        let mut dir: Directory = HashMap::new();
        match self.next_ifd {
            None => return Err(ImageError::FormatError(
                "Image file directory not found.".to_string())
            ),
            Some(offset) => try!(self.goto_offset(offset))
        }
        for _ in 0..try!(self.read_short()) {
            let (tag, entry) = match try!(self.read_entry()) {
                Some(val) => val,
                None => continue // Unknown data type in tag, skip
            };
            dir.insert(tag, entry);
        }
        self.next_ifd = match try!(self.read_long()) {
            0 => None,
            n => Some(n)
        };
        Ok(dir)
    }

    /// Tries to retrieve a tag.
    /// Return `Ok(None)` if the tag is not present.
    fn find_tag(&mut self, tag: ifd::Tag) -> ImageResult<Option<ifd::Value>> {
        let ifd: &Directory = unsafe {
            let ifd = self.ifd.as_ref().unwrap(); // Ok to fail
            // Get a immutable borrow of self
            // This is ok because entry val only changes the stream
            // but not the directory.
            mem::transmute_copy(&ifd)
        };
        match ifd.get(&tag) {
            None => Ok(None),
            Some(entry) => Ok(Some(try!(entry.val(self))))
        }
    }

    /// Tries to retrieve a tag and convert it to the desired type.
    fn find_tag_u32(&mut self, tag: ifd::Tag) -> ImageResult<Option<u32>> {
        match self.find_tag(tag)? {
            Some(val) => val.into_u32().map(Some),
            None => Ok(None)
        }
    }

    /// Tries to retrieve a tag and convert it to the desired type.
    fn find_tag_u32_vec(&mut self, tag: ifd::Tag) -> ImageResult<Option<Vec<u32>>> {
        match self.find_tag(tag)? {
            Some(val) => val.into_u32_vec().map(Some),
            None => Ok(None)
        }
    }

    /// Tries to retrieve a tag.
    /// Returns an error if the tag is not present
    fn get_tag(&mut self, tag: ifd::Tag) -> ImageResult<ifd::Value> {
        match try!(self.find_tag(tag)) {
            Some(val) => Ok(val),
            None => Err(ImageError::FormatError(format!(
                "Required tag `{:?}` not found.", tag
            )))
        }
    }

    /// Tries to retrieve a tag and convert it to the desired type.
    fn get_tag_u32(&mut self, tag: ifd::Tag) -> ImageResult<u32> {
        self.get_tag(tag)?.into_u32()
    }

    /// Tries to retrieve a tag and convert it to the desired type.
    fn get_tag_u32_vec(&mut self, tag: ifd::Tag) -> ImageResult<Vec<u32>> {
        self.get_tag(tag)?.into_u32_vec()
    }

    /// Decompresses the strip into the supplied buffer.
    /// Returns the number of bytes read.
    fn expand_strip<'a>(&mut self, buffer: DecodingBuffer<'a>, offset: u32, length: u32, max_uncompressed_length: usize) -> ImageResult<usize> {
        let color_type = try!(self.colortype());
        try!(self.goto_offset(offset));
        let (bytes, mut reader): (usize, Box<EndianReader>) = match self.compression_method {
            CompressionMethod::None => {
                let order = self.reader.byte_order;
                (length as usize, Box::new(SmartReader::wrap(&mut self.reader, order)))
            },
            CompressionMethod::LZW => {
                let (bytes, reader) = try!(LZWReader::new(&mut self.reader, length as usize, max_uncompressed_length));
                (bytes, Box::new(reader))
            },
            CompressionMethod::PackBits => {
                let order = self.reader.byte_order;
                let (bytes, reader) = try!(PackBitsReader::new(&mut self.reader, order, length as usize));
                (bytes, Box::new(reader))
            },
            method => return Err(ImageError::UnsupportedError(format!(
                "Compression method {:?} is unsupported", method
            )))
        };
        Ok(match (color_type, buffer) {
            (ColorType:: RGB(8), DecodingBuffer::U8(ref mut buffer)) |
            (ColorType::RGBA(8), DecodingBuffer::U8(ref mut buffer)) => {
                try!(reader.read(&mut buffer[..bytes]))
            }
            (ColorType::RGBA(16), DecodingBuffer::U16(ref mut buffer)) |
            (ColorType:: RGB(16), DecodingBuffer::U16(ref mut buffer)) => {
                for datum in buffer[..bytes/2].iter_mut() {
                    *datum = try!(reader.read_u16())
                }
                bytes/2
            }
            (ColorType::Gray(16), DecodingBuffer::U16(ref mut buffer)) => {
                for datum in buffer[..bytes/2].iter_mut() {
                    *datum = try!(reader.read_u16());
                    if self.photometric_interpretation == PhotometricInterpretation::WhiteIsZero {
                        *datum = 0xffff - *datum
                    }
                }
                bytes/2
            }
            (ColorType::Gray(n), DecodingBuffer::U8(ref mut buffer)) if n <= 8 => {
                try!(reader.read_exact(&mut buffer[..bytes]));
                if self.photometric_interpretation == PhotometricInterpretation::WhiteIsZero {
                    for byte in buffer[..bytes].iter_mut() {
                        *byte = 0xff - *byte
                    }
                }
                bytes
            }
            (type_, _) => return Err(ImageError::UnsupportedError(format!(
                "Color type {:?} is unsupported", type_
            )))
        })
    }
}
