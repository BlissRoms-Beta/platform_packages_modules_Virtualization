// Copyright 2023, The Android Open Source Project
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Contains struct and functions that wraps the API related to EC_KEY in
//! BoringSSL.

use alloc::vec::Vec;
use bssl_ffi::BN_bn2bin_padded;
use bssl_ffi::BN_clear_free;
use bssl_ffi::BN_new;
use bssl_ffi::CBB_flush;
use bssl_ffi::CBB_init_fixed;
use bssl_ffi::CBB_len;
use bssl_ffi::EC_KEY_free;
use bssl_ffi::EC_KEY_generate_key;
use bssl_ffi::EC_KEY_get0_group;
use bssl_ffi::EC_KEY_get0_public_key;
use bssl_ffi::EC_KEY_marshal_private_key;
use bssl_ffi::EC_KEY_new_by_curve_name;
use bssl_ffi::EC_POINT_get_affine_coordinates;
use bssl_ffi::NID_X9_62_prime256v1; // EC P-256 CURVE Nid
use bssl_ffi::BIGNUM;
use bssl_ffi::EC_GROUP;
use bssl_ffi::EC_KEY;
use bssl_ffi::EC_POINT;
use core::mem::MaybeUninit;
use core::ptr::{self, NonNull};
use core::result;
use coset::{iana, CoseKey, CoseKeyBuilder};
use service_vm_comm::{BoringSSLApiName, RequestProcessingError};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

const P256_AFFINE_COORDINATE_SIZE: usize = 32;

type Result<T> = result::Result<T, RequestProcessingError>;
type Coordinate = [u8; P256_AFFINE_COORDINATE_SIZE];

/// Wrapper of an `EC_KEY` object, representing a public or private EC key.
pub struct EcKey(NonNull<EC_KEY>);

impl Drop for EcKey {
    fn drop(&mut self) {
        // SAFETY: It is safe because the key has been allocated by BoringSSL and isn't
        // used after this.
        unsafe { EC_KEY_free(self.0.as_ptr()) }
    }
}

impl EcKey {
    /// Creates a new EC P-256 key pair.
    pub fn new_p256() -> Result<Self> {
        // SAFETY: The returned pointer is checked below.
        let ec_key = unsafe { EC_KEY_new_by_curve_name(NID_X9_62_prime256v1) };
        let mut ec_key = NonNull::new(ec_key).map(Self).ok_or(
            RequestProcessingError::BoringSSLCallFailed(BoringSSLApiName::EC_KEY_new_by_curve_name),
        )?;
        ec_key.generate_key()?;
        Ok(ec_key)
    }

    /// Generates a random, private key, calculates the corresponding public key and stores both
    /// in the `EC_KEY`.
    fn generate_key(&mut self) -> Result<()> {
        // SAFETY: The non-null pointer is created with `EC_KEY_new_by_curve_name` and should
        // point to a valid `EC_KEY`.
        // The randomness is provided by `getentropy()` in `vmbase`.
        let ret = unsafe { EC_KEY_generate_key(self.0.as_ptr()) };
        check_int_result(ret, BoringSSLApiName::EC_KEY_generate_key)
    }

    /// Returns the `CoseKey` for the public key.
    pub fn cose_public_key(&self) -> Result<CoseKey> {
        const ALGO: iana::Algorithm = iana::Algorithm::ES256;
        const CURVE: iana::EllipticCurve = iana::EllipticCurve::P_256;

        let (x, y) = self.public_key_coordinates()?;
        let key =
            CoseKeyBuilder::new_ec2_pub_key(CURVE, x.to_vec(), y.to_vec()).algorithm(ALGO).build();
        Ok(key)
    }

    /// Returns the x and y coordinates of the public key.
    fn public_key_coordinates(&self) -> Result<(Coordinate, Coordinate)> {
        let ec_group = self.ec_group()?;
        let ec_point = self.public_key_ec_point()?;
        let mut x = BigNum::new()?;
        let mut y = BigNum::new()?;
        let ctx = ptr::null_mut();
        // SAFETY: All the parameters are checked non-null and initialized when needed.
        // The last parameter `ctx` is generated when needed inside the function.
        let ret = unsafe {
            EC_POINT_get_affine_coordinates(ec_group, ec_point, x.as_mut_ptr(), y.as_mut_ptr(), ctx)
        };
        check_int_result(ret, BoringSSLApiName::EC_POINT_get_affine_coordinates)?;
        Ok((x.try_into()?, y.try_into()?))
    }

    /// Returns a pointer to the public key point inside `EC_KEY`. The memory region pointed
    /// by the pointer is owned by the `EC_KEY`.
    fn public_key_ec_point(&self) -> Result<*const EC_POINT> {
        let ec_point =
           // SAFETY: It is safe since the key pair has been generated and stored in the
           // `EC_KEY` pointer.
           unsafe { EC_KEY_get0_public_key(self.0.as_ptr()) };
        if ec_point.is_null() {
            Err(RequestProcessingError::BoringSSLCallFailed(
                BoringSSLApiName::EC_KEY_get0_public_key,
            ))
        } else {
            Ok(ec_point)
        }
    }

    /// Returns a pointer to the `EC_GROUP` object inside `EC_KEY`. The memory region pointed
    /// by the pointer is owned by the `EC_KEY`.
    fn ec_group(&self) -> Result<*const EC_GROUP> {
        let group =
           // SAFETY: It is safe since the key pair has been generated and stored in the
           // `EC_KEY` pointer.
           unsafe { EC_KEY_get0_group(self.0.as_ptr()) };
        if group.is_null() {
            Err(RequestProcessingError::BoringSSLCallFailed(BoringSSLApiName::EC_KEY_get0_group))
        } else {
            Ok(group)
        }
    }

    /// Returns the DER-encoded ECPrivateKey structure described in RFC 5915 Section 3:
    ///
    /// https://datatracker.ietf.org/doc/html/rfc5915#section-3
    pub fn private_key(&self) -> Result<ZVec> {
        const CAPACITY: usize = 256;
        let mut buf = Zeroizing::new([0u8; CAPACITY]);
        // SAFETY: `CBB_init_fixed()` is infallible and always returns one.
        // The `buf` is never moved and remains valid during the lifetime of `cbb`.
        let mut cbb = unsafe {
            let mut cbb = MaybeUninit::uninit();
            CBB_init_fixed(cbb.as_mut_ptr(), buf.as_mut_ptr(), buf.len());
            cbb.assume_init()
        };
        let enc_flags = 0;
        let ret =
            // SAFETY: The function only write bytes to the buffer managed by the valid `CBB`
            // object, and the key has been allocated by BoringSSL.
            unsafe { EC_KEY_marshal_private_key(&mut cbb, self.0.as_ptr(), enc_flags) };

        check_int_result(ret, BoringSSLApiName::EC_KEY_marshal_private_key)?;
        // SAFETY: This is safe because the CBB pointer is a valid pointer initialized with
        // `CBB_init_fixed()`.
        check_int_result(unsafe { CBB_flush(&mut cbb) }, BoringSSLApiName::CBB_flush)?;
        // SAFETY: This is safe because the CBB pointer is initialized with `CBB_init_fixed()`,
        // and it has been flushed, thus it has no active children.
        let len = unsafe { CBB_len(&cbb) };
        Ok(buf
            .get(0..len)
            .ok_or(RequestProcessingError::BoringSSLCallFailed(BoringSSLApiName::CBB_len))?
            .to_vec()
            .into())
    }
}

/// A u8 vector that is zeroed when dropped.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct ZVec(Vec<u8>);

impl ZVec {
    /// Extracts a slice containing the entire vector.
    pub fn as_slice(&self) -> &[u8] {
        &self.0[..]
    }
}

impl From<Vec<u8>> for ZVec {
    fn from(v: Vec<u8>) -> Self {
        Self(v)
    }
}

struct BigNum(NonNull<BIGNUM>);

impl Drop for BigNum {
    fn drop(&mut self) {
        // SAFETY: The pointer has been created with `BN_new`.
        unsafe { BN_clear_free(self.as_mut_ptr()) }
    }
}

impl BigNum {
    fn new() -> Result<Self> {
        // SAFETY: The returned pointer is checked below.
        let bn = unsafe { BN_new() };
        NonNull::new(bn)
            .map(Self)
            .ok_or(RequestProcessingError::BoringSSLCallFailed(BoringSSLApiName::BN_new))
    }

    fn as_mut_ptr(&mut self) -> *mut BIGNUM {
        self.0.as_ptr()
    }
}

/// Converts the `BigNum` to a big-endian integer. The integer is padded with leading zeros up to
/// size `N`. The conversion fails if `N` is smaller thanthe size of the integer.
impl<const N: usize> TryFrom<BigNum> for [u8; N] {
    type Error = RequestProcessingError;

    fn try_from(bn: BigNum) -> result::Result<Self, Self::Error> {
        let mut num = [0u8; N];
        // SAFETY: The `BIGNUM` pointer has been created with `BN_new`.
        let ret = unsafe { BN_bn2bin_padded(num.as_mut_ptr(), num.len(), bn.0.as_ptr()) };
        check_int_result(ret, BoringSSLApiName::BN_bn2bin_padded)?;
        Ok(num)
    }
}

fn check_int_result(ret: i32, api_name: BoringSSLApiName) -> Result<()> {
    if ret == 1 {
        Ok(())
    } else {
        assert_eq!(ret, 0, "Unexpected return value {ret} for {api_name:?}");
        Err(RequestProcessingError::BoringSSLCallFailed(api_name))
    }
}

// TODO(b/301068421): Unit tests the EcKey.
