// This file is derived from revm (MIT licensed)
// Copyright (c) 2021-2025 draganrakita
// Modified by Conflux Foundation 2025

use super::{
    consts::{G1_INPUT_ITEM_LENGTH, G1_OUTPUT_LENGTH, PADDED_FP_LENGTH},
    utils::{fp_from_bendian, fp_to_bytes, remove_padding},
};
use blst::{blst_p1_affine, blst_p1_affine_in_g1, blst_p1_affine_on_curve};

/// Encodes a G1 point in affine format into byte slice with padded elements.
pub(super) fn encode_g1_point(input: *const blst_p1_affine) -> Vec<u8> {
    let mut out = vec![0u8; G1_OUTPUT_LENGTH];
    // SAFETY: Out comes from fixed length array, input is a blst value.
    unsafe {
        fp_to_bytes(&mut out[..PADDED_FP_LENGTH], &(*input).x);
        fp_to_bytes(&mut out[PADDED_FP_LENGTH..], &(*input).y);
    }
    out
}

/// Returns a `blst_p1_affine` from the provided byte slices, which represent
/// the x and y affine coordinates of the point.
///
/// If the x or y coordinate do not represent a canonical field element, an
/// error is returned.
///
/// See [fp_from_bendian] for more information.
pub(super) fn decode_and_check_g1(
    p0_x: &[u8; 48], p0_y: &[u8; 48],
) -> Result<blst_p1_affine, String> {
    let out = blst_p1_affine {
        x: fp_from_bendian(p0_x)?,
        y: fp_from_bendian(p0_y)?,
    };

    Ok(out)
}

/// Extracts a G1 point in Affine format from a 128 byte slice representation.
///
/// **Note**: This function will perform a G1 subgroup check if `subgroup_check`
/// is set to `true`.
pub(super) fn extract_g1_input(
    input: &[u8], subgroup_check: bool,
) -> Result<blst_p1_affine, String> {
    if input.len() != G1_INPUT_ITEM_LENGTH {
        return Err(format!(
            "Input should be {G1_INPUT_ITEM_LENGTH} bytes, was {}",
            input.len()
        ));
    }

    let input_p0_x = remove_padding(&input[..PADDED_FP_LENGTH])?;
    let input_p0_y =
        remove_padding(&input[PADDED_FP_LENGTH..G1_INPUT_ITEM_LENGTH])?;
    let out = decode_and_check_g1(input_p0_x, input_p0_y)?;

    if subgroup_check {
        // NB: Subgroup checks
        //
        // Scalar multiplications, MSMs and pairings MUST perform a subgroup
        // check.
        //
        // Implementations SHOULD use the optimized subgroup check method:
        //
        // https://eips.ethereum.org/assets/eip-2537/fast_subgroup_checks
        //
        // On any input that fail the subgroup check, the precompile MUST return
        // an error.
        //
        // As endomorphism acceleration requires input on the correct subgroup,
        // implementers MAY use endomorphism acceleration.
        if unsafe { !blst_p1_affine_in_g1(&out) } {
            return Err("Element not in G1".to_string());
        }
    } else {
        // From EIP-2537:
        //
        // Error cases:
        //
        // * An input is neither a point on the G1 elliptic curve nor the
        //   infinity point
        //
        // NB: There is no subgroup check for the G1 addition precompile.
        //
        // We use blst_p1_affine_on_curve instead of blst_p1_affine_in_g1
        // because the latter performs the subgroup check.
        //
        // SAFETY: Out is a blst value.
        if unsafe { !blst_p1_affine_on_curve(&out) } {
            return Err("Element not on G1 curve".to_string());
        }
    }

    Ok(out)
}
