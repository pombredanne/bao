//! The tests in this file run bao against the standard set of test vectors.

extern crate bao;
extern crate blake2b_simd;
extern crate byteorder;
#[macro_use]
extern crate lazy_static;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;

use bao::hash::Hash;
use byteorder::{ByteOrder, LittleEndian};
use std::cmp;
use std::io;
use std::io::prelude::*;
use std::io::Cursor;

lazy_static! {
    static ref TEST_VECTORS: TestVectors =
        serde_json::from_str(include_str!("test_vectors.json")).unwrap();
}

#[derive(Debug, Serialize, Deserialize)]
struct TestVectors {
    hash: Vec<HashTest>,
    encode: Vec<EncodeTest>,
    outboard: Vec<OutboardTest>,
    seek: Vec<SeekTest>,
    slice: Vec<SliceTest>,
}

#[derive(Debug, Serialize, Deserialize)]
struct HashTest {
    input_len: usize,
    bao_hash: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct EncodeTest {
    input_len: usize,
    output_len: usize,
    bao_hash: String,
    encoded_blake2b: String,
    corruptions: Vec<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OutboardTest {
    input_len: usize,
    output_len: usize,
    bao_hash: String,
    encoded_blake2b: String,
    outboard_corruptions: Vec<usize>,
    input_corruptions: Vec<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SeekTest {
    input_len: usize,
    seek_offsets: Vec<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SliceTest {
    input_len: usize,
    bao_hash: String,
    slices: Vec<SliceTestSlice>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SliceTestSlice {
    start: u64,
    len: u64,
    output_len: usize,
    output_blake2b: String,
    corruptions: Vec<usize>,
}

fn make_input(len: usize) -> Vec<u8> {
    let mut counter: u32 = 1;
    let mut output = Vec::with_capacity(len);
    while output.len() < len {
        let mut bytes = [0; 4];
        LittleEndian::write_u32(&mut bytes, counter);
        let take = cmp::min(4, len - output.len());
        output.extend_from_slice(&bytes[..take]);
        counter += 1;
    }
    output
}

fn blake2b(bytes: &[u8]) -> String {
    blake2b_simd::Params::new()
        .hash_length(16)
        .to_state()
        .update(bytes)
        .finalize()
        .to_hex()
        .to_string()
}

#[test]
fn test_hash_vectors() {
    for case in &TEST_VECTORS.hash {
        println!("case {:?}", case);
        let input = make_input(case.input_len);
        let hash = bao::hash::hash(&input);
        assert_eq!(case.bao_hash, hash.to_hex().to_string());

        // Make sure the Writer gives the same answer.
        let mut writer = bao::hash::Writer::new();
        writer.write_all(&input).unwrap();
        let writer_hash = writer.finish();
        assert_eq!(hash, writer_hash);

        // Make sure the ParallelWriter gives the same answer.
        let mut writer = bao::hash::ParallelWriter::new();
        writer.write_all(&input).unwrap();
        let writer_hash = writer.finish();
        assert_eq!(hash, writer_hash);
    }
}

fn all_decode_implementations(encoded: &[u8], hash: &Hash) -> Result<Vec<u8>, bao::decode::Error> {
    let content_len = bao::decode::parse_and_check_content_len(encoded)?;
    let mut output = vec![0; content_len];
    let result = bao::decode::decode(&encoded, &mut output, &hash).map(|_| output);

    // Make sure all the other implementations of decode give the same answer.
    {
        let to_vec_result = bao::decode::decode_to_vec(&encoded, &hash);
        assert_eq!(result, to_vec_result);

        let mut in_place = encoded.to_vec();
        let in_place_result = bao::decode::decode_in_place(&mut in_place, &hash);
        assert_eq!(
            result,
            in_place_result.map(|_| in_place[..content_len].to_vec())
        );

        let mut output = Vec::new();
        let mut reader = bao::decode::Reader::new(&*encoded, &hash);
        let reader_result = reader.read_to_end(&mut output);
        match (&result, &reader_result) {
            (&Ok(ref expected_output), &Ok(_)) => assert_eq!(expected_output, &output),
            (&Err(expected_error), &Err(ref found_error)) => {
                let expected_io_err: std::io::Error = expected_error.into();
                assert_eq!(expected_io_err.kind(), found_error.kind());
                assert_eq!(expected_io_err.to_string(), found_error.to_string());
            }
            _ => panic!("mismatch"),
        }
    }

    result
}

fn corrupt_hash(hash: &Hash) -> Hash {
    let mut bad_bytes = *hash.as_bytes();
    bad_bytes[0] ^= 1;
    Hash::new(bad_bytes)
}

#[test]
fn test_encode_vectors() {
    for case in &TEST_VECTORS.encode {
        println!("input_len {}", case.input_len);
        let input = make_input(case.input_len);
        let encoded_size = bao::encode::encoded_size(case.input_len as u64) as usize;
        let mut encoded = vec![0; encoded_size];
        let hash = bao::encode::encode(&input, &mut encoded);

        // Make sure the encoded hash is correct.
        assert_eq!(case.bao_hash, hash.to_hex().to_string());

        // Make sure the encoded output is correct.
        assert_eq!(case.encoded_blake2b, blake2b(&encoded));

        // Make sure all the other implementations of encode give the same answer.
        {
            let (to_vec_hash, to_vec) = bao::encode::encode_to_vec(&input);
            assert_eq!(hash, to_vec_hash);
            assert_eq!(encoded, to_vec);

            let mut in_place = vec![0; encoded_size];
            in_place[..case.input_len].copy_from_slice(&input);
            let in_place_hash = bao::encode::encode_in_place(&mut in_place, case.input_len);
            assert_eq!(hash, in_place_hash);
            assert_eq!(encoded, in_place);

            let mut output = Vec::new();
            {
                let mut writer = bao::encode::Writer::new(Cursor::new(&mut output));
                writer.write_all(&input).unwrap();
                let writer_hash = writer.finish().unwrap();
                assert_eq!(hash, writer_hash);
            }
            assert_eq!(encoded, output);
        }

        // Test getting the hash from the encoding.
        let hash_encoded = bao::decode::hash_from_encoded(&mut &*encoded).unwrap();
        assert_eq!(hash, hash_encoded);

        // Test decoding.
        let output = all_decode_implementations(&encoded, &hash).unwrap();
        assert_eq!(input, output);

        // Make sure decoding with a bad hash fails.
        let bad_hash = corrupt_hash(&hash);
        let err = all_decode_implementations(&encoded, &bad_hash).unwrap_err();
        assert_eq!(bao::decode::Error::HashMismatch, err);

        // Make sure each corruption point fails the decode.
        for &point in &case.corruptions {
            println!("corruption {}", point);
            let mut corrupt = encoded.clone();
            corrupt[point] ^= 1;
            // The error can be either HashMismatch or Truncated, depending on whether the header
            // was corrupted.
            all_decode_implementations(&corrupt, &hash).unwrap_err();
        }
    }
}

fn decode_outboard(input: &[u8], outboard: &[u8], hash: &Hash) -> io::Result<Vec<u8>> {
    let mut reader = bao::decode::Reader::new_outboard(input, outboard, hash);
    let mut output = Vec::with_capacity(input.len());
    reader.read_to_end(&mut output)?;
    Ok(output)
}

#[test]
fn test_outboard_vectors() {
    for case in &TEST_VECTORS.outboard {
        println!("input_len {}", case.input_len);
        let input = make_input(case.input_len);
        let encoded_size = bao::encode::outboard_size(case.input_len as u64) as usize;
        let mut outboard = vec![0; encoded_size];
        let hash = bao::encode::encode_outboard(&input, &mut outboard);

        // Make sure the encoded hash is correct.
        assert_eq!(case.bao_hash, hash.to_hex().to_string());

        // Make sure the outboard output is correct.
        assert_eq!(case.encoded_blake2b, blake2b(&outboard));

        // Make sure all the other implementations of encode give the same answer.
        {
            let (to_vec_hash, to_vec) = bao::encode::encode_outboard_to_vec(&input);
            assert_eq!(hash, to_vec_hash);
            assert_eq!(outboard, to_vec);

            let mut output = Vec::new();
            {
                let mut writer = bao::encode::Writer::new_outboard(Cursor::new(&mut output));
                writer.write_all(&input).unwrap();
                let writer_hash = writer.finish().unwrap();
                assert_eq!(hash, writer_hash);
            }
            assert_eq!(outboard, output);
        }

        // Test getting the hash from the encoding.
        let hash_encoded =
            bao::decode::hash_from_outboard_encoded(&mut &*input, &mut &*outboard).unwrap();
        assert_eq!(hash, hash_encoded);

        // Test decoding. Currently only the Reader implements it.
        let output = decode_outboard(&input, &outboard, &hash).unwrap();
        assert_eq!(input, output);

        // Make sure decoding with a bad hash fails.
        let bad_hash = corrupt_hash(&hash);
        let err = decode_outboard(&input, &outboard, &bad_hash).unwrap_err();
        assert_eq!(io::ErrorKind::InvalidData, err.kind());

        // Make sure each tree corruption point fails the decode.
        for &point in &case.outboard_corruptions {
            println!("corruption {}", point);
            let mut corrupt = outboard.clone();
            corrupt[point] ^= 1;
            // The error can be either InvalidData or UnexpectedEof, depending on whether the
            // header was corrupted.
            decode_outboard(&input, &corrupt, &hash).unwrap_err();
        }

        // Make sure each input corruption point fails the decode.
        for &point in &case.input_corruptions {
            println!("corruption {}", point);
            let mut corrupt = input.clone();
            corrupt[point] ^= 1;
            let err = decode_outboard(&corrupt, &outboard, &hash).unwrap_err();
            assert_eq!(io::ErrorKind::InvalidData, err.kind());
        }
    }
}

#[test]
fn test_seek_vectors() {
    for case in &TEST_VECTORS.seek {
        println!("\n\ninput_len {}", case.input_len);
        let input = make_input(case.input_len);
        let encoded_size = bao::encode::encoded_size(case.input_len as u64) as usize;
        let mut encoded = vec![0; encoded_size];
        let hash = bao::encode::encode(&input, &mut encoded);
        let outboard_size = bao::encode::outboard_size(case.input_len as u64) as usize;
        let mut outboard = vec![0; outboard_size];
        let outboard_hash = bao::encode::encode_outboard(&input, &mut outboard);
        assert_eq!(hash, outboard_hash);

        // First, test all the different seek points using fresh readers.
        println!();
        for &seek in &case.seek_offsets {
            println!("seek {}", seek);
            let capped_seek = cmp::min(seek, input.len());
            let expected_input = &input[capped_seek..];

            // Test seeking in the combined mode.
            let mut combined_reader = bao::decode::Reader::new(Cursor::new(&encoded), &hash);
            combined_reader
                .seek(io::SeekFrom::Start(seek as u64))
                .unwrap();
            let mut combined_output = Vec::new();
            combined_reader.read_to_end(&mut combined_output).unwrap();
            assert_eq!(expected_input, &*combined_output);

            // Test seeking in the outboard mode.
            let mut outboard_reader = bao::decode::Reader::new_outboard(
                Cursor::new(&input),
                Cursor::new(&outboard),
                &hash,
            );
            outboard_reader
                .seek(io::SeekFrom::Start(seek as u64))
                .unwrap();
            let mut combined_output = Vec::new();
            outboard_reader.read_to_end(&mut combined_output).unwrap();
            assert_eq!(expected_input, &*combined_output);
        }

        // Then, test repeatedly seeking using the same reader. First, iterate forwards through the
        // list of seek positions. Then, iterate backwards. Finally, iterate interleaved between
        // forwards and backwards. At each step, read a few bytes as a sanity check.
        let mut repeated_seeks: Vec<usize> = Vec::new();
        repeated_seeks.extend(case.seek_offsets.iter());
        repeated_seeks.extend(case.seek_offsets.iter().rev());
        for (&x, &y) in case.seek_offsets.iter().zip(case.seek_offsets.iter().rev()) {
            repeated_seeks.push(x);
            repeated_seeks.push(y);
        }
        let mut combined_reader = bao::decode::Reader::new(Cursor::new(&encoded), &hash);
        let mut outboard_reader =
            bao::decode::Reader::new_outboard(Cursor::new(&input), Cursor::new(&outboard), &hash);
        println!();
        for &seek in &repeated_seeks {
            println!("repeated seek {}", seek);
            let capped_seek = cmp::min(seek, input.len());
            let capped_len = cmp::min(100, input.len() - capped_seek);
            let mut read_buf = [0; 100];

            // Test seeking in the combined mode.
            combined_reader
                .seek(io::SeekFrom::Start(seek as u64))
                .unwrap();
            combined_reader
                .read_exact(&mut read_buf[..capped_len])
                .unwrap();
            assert_eq!(&input[capped_seek..][..capped_len], &read_buf[..capped_len]);

            // Test seeking in the outboard mode.
            outboard_reader
                .seek(io::SeekFrom::Start(seek as u64))
                .unwrap();
            outboard_reader
                .read_exact(&mut read_buf[..capped_len])
                .unwrap();
            assert_eq!(&input[capped_seek..][..capped_len], &read_buf[..capped_len]);
        }
    }
}

fn decode_slice(slice: &[u8], hash: &Hash, start: u64, len: u64) -> io::Result<Vec<u8>> {
    let mut reader = bao::decode::SliceReader::new(slice, hash, start, len);
    let mut output = Vec::new();
    reader.read_to_end(&mut output)?;
    Ok(output)
}

#[test]
fn test_slice_vectors() {
    for case in &TEST_VECTORS.slice {
        println!("\n\ninput_len {}", case.input_len);
        let input = make_input(case.input_len);
        let encoded_size = bao::encode::encoded_size(case.input_len as u64) as usize;
        let mut encoded = vec![0; encoded_size];
        let hash = bao::encode::encode(&input, &mut encoded);
        let outboard_size = bao::encode::outboard_size(case.input_len as u64) as usize;
        let mut outboard = vec![0; outboard_size];
        let outboard_hash = bao::encode::encode_outboard(&input, &mut outboard);
        assert_eq!(hash, outboard_hash);

        for slice in &case.slices {
            println!("\nslice {} {}", slice.start, slice.len);
            let capped_start = cmp::min(input.len(), slice.start as usize);
            let capped_len = cmp::min(input.len() - capped_start, slice.len as usize);
            let expected_content = &input[capped_start..][..capped_len];

            // Make sure slicing the combined encoding has the output that it should.
            let mut combined_extractor =
                bao::encode::SliceExtractor::new(Cursor::new(&encoded), slice.start, slice.len);
            let mut combined_slice = Vec::new();
            combined_extractor.read_to_end(&mut combined_slice).unwrap();
            assert_eq!(slice.output_len, combined_slice.len());
            assert_eq!(slice.output_blake2b, blake2b(&combined_slice));

            // Make sure slicing the outboard encoding also gives the right output.
            let mut outboard_extractor = bao::encode::SliceExtractor::new_outboard(
                Cursor::new(&input),
                Cursor::new(&outboard),
                slice.start,
                slice.len,
            );
            let mut outboard_slice = Vec::new();
            outboard_extractor.read_to_end(&mut outboard_slice).unwrap();
            assert_eq!(combined_slice, outboard_slice);

            // Test decoding the slice.
            let output = decode_slice(&combined_slice, &hash, slice.start, slice.len).unwrap();
            assert_eq!(expected_content, &*output);

            // Make sure that using the wrong hash breaks decoding.
            let bad_hash = corrupt_hash(&hash);
            let err = decode_slice(&combined_slice, &bad_hash, slice.start, slice.len).unwrap_err();
            assert_eq!(io::ErrorKind::InvalidData, err.kind());

            // Test that each of the corruption points breaks decoding the slice.
            for &point in &slice.corruptions {
                println!("corruption {}", point);
                let mut corrupted = combined_slice.clone();
                corrupted[point] ^= 1;
                // The error can be either HashMismatch or Truncated, depending on whether the header
                // was corrupted.
                decode_slice(&corrupted, &hash, slice.start, slice.len).unwrap_err();
            }
        }
    }
}
