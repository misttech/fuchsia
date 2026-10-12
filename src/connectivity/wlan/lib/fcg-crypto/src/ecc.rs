// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::boringssl::{self, Bignum, BignumCtx, EcGroup, EcGroupId, EcPoint};
use crate::fcg::FiniteCyclicGroup;
use crate::sae::{PweMethod, SaeParameters};
use anyhow::{Error, bail};
use ieee80211::MacAddr;
use log::warn;
use num_traits::ToPrimitive;

/// An elliptic curve group to be used as the finite cyclic group for SAE.
pub struct Group {
    id: EcGroupId,
    group: EcGroup,
    bn_ctx: BignumCtx,
}

impl Group {
    /// Construct a new FCG using the curve parameters specified by the given ID
    /// for underlying operations.
    pub fn new(ec_group: EcGroupId) -> Result<Self, Error> {
        Ok(Self { id: ec_group.clone(), group: EcGroup::new(ec_group)?, bn_ctx: BignumCtx::new()? })
    }
}

/// Concatenates two MAC addresses in canonical order (largest one first)
fn concat_mac_addrs(sta_a_mac: &MacAddr, sta_b_mac: &MacAddr) -> Vec<u8> {
    let mut result: Vec<u8> = Vec::with_capacity(sta_a_mac.len() + sta_b_mac.len());
    match sta_a_mac.cmp(sta_b_mac) {
        std::cmp::Ordering::Less => {
            result.extend_from_slice(sta_b_mac.as_slice());
            result.extend_from_slice(sta_a_mac.as_slice());
        }
        _ => {
            result.extend_from_slice(sta_a_mac.as_slice());
            result.extend_from_slice(sta_b_mac.as_slice());
        }
    };

    result
}

// The minimum number of times we must attempt to generate a PWE to avoid timing attacks.
// IEEE Std 802.11-2016 12.4.4.2.2 states that this number should be high enough that the
// probability of not finding a PWE is "sufficiently small". For a given odd prime modulus, there
// are (p + 1) / 2 quadratic residues. This means that for large p, there's a ~50% chance of
// finding a residue on each PWE iteration, so the probability of exceeding our number of iters is
// (1/2)^MIN_PWE_ITER. At 50 iterations, that's about 1 in 80 quadrillion... Seems sufficient.
const MIN_PWE_ITER: u8 = 50;

impl Group {
    // IEEE Std 802.11-2020 12.4.4.2.2
    fn generate_pwe_loop(&self, params: &SaeParameters) -> Result<EcPoint, Error> {
        if params.password_id.is_some() {
            // IEEE Std 802.11-2020 12.4.4.3.2
            bail!("Password ID cannot be used with looping PWE generation");
        }

        let salt = concat_mac_addrs(&params.sta_a_mac, &params.sta_b_mac);
        EcPoint::new_wpa3_sae_hunt_and_peck_p256(&self.group, &salt, &params.password, MIN_PWE_ITER)
    }

    /// IEEE Std 802.11-2020 12.4.4.2.3
    /// Returns the secret PT used to generate the PWE.
    fn generate_pt(&self, params: &SaeParameters) -> Result<EcPoint, Error> {
        let mut password_with_id = params.password.clone();
        if let Some(password_id) = &params.password_id {
            password_with_id.extend_from_slice(password_id);
        }
        EcPoint::new_wpa3_sae_hash_to_curve_p256(&self.group, &params.ssid, &password_with_id)
    }

    // IEEE Std 802.11-2020 12.4.5.2
    fn generate_pwe_direct(&self, params: &SaeParameters) -> Result<EcPoint, Error> {
        // The secret element pt is used for each connection on this SSID and password, and could
        // potentially be cached and reused.  However, in actual use we generate a new Group
        // instance for each call to generate_pwe(), so we do not do any caching here.
        let pt = self.generate_pt(params)?;

        // Now generate the PWE from the PT and MAC addresses.
        let salt = vec![0u8; params.hmac.bits() / 8];
        let ikm = concat_mac_addrs(&params.sta_a_mac, &params.sta_b_mac);
        let val = Bignum::new_from_slice(&params.hmac.hkdf_extract(&salt, &ikm))?;
        let val = val
            .mod_nonnegative(
                &self.group.get_order(&self.bn_ctx)?.sub(Bignum::one()?)?,
                &self.bn_ctx,
            )?
            .add(Bignum::one()?)?;

        self.scalar_op(&val, &pt)
    }
}

impl FiniteCyclicGroup for Group {
    type Element = boringssl::EcPoint;

    fn group_id(&self) -> u16 {
        self.id.to_u16().unwrap()
    }

    fn generate_pwe(&self, params: &SaeParameters) -> Result<Self::Element, Error> {
        match params.pwe_method {
            PweMethod::Loop => self.generate_pwe_loop(params),
            PweMethod::Direct => self.generate_pwe_direct(params),
        }
    }

    fn scalar_op(&self, scalar: &Bignum, element: &Self::Element) -> Result<Self::Element, Error> {
        element.mul(&self.group, &scalar, &self.bn_ctx)
    }

    fn elem_op(
        &self,
        element1: &Self::Element,
        element2: &Self::Element,
    ) -> Result<Self::Element, Error> {
        element1.add(&self.group, &element2, &self.bn_ctx)
    }

    fn inverse_op(&self, element: Self::Element) -> Result<Self::Element, Error> {
        element.invert(&self.group, &self.bn_ctx)
    }

    fn order(&self) -> Result<Bignum, Error> {
        self.group.get_order(&self.bn_ctx)
    }

    fn generator(&self) -> Result<EcPoint, Error> {
        self.group.get_generator()
    }

    fn map_to_secret_value(&self, element: &Self::Element) -> Result<Option<Vec<u8>>, Error> {
        // IEEE Std 802.11-2016 12.4.4.2.1 (end of section)
        if element.is_point_at_infinity(&self.group) {
            Ok(None)
        } else {
            let length = self.group.get_degree().div_ceil(8);
            let (x, _y) = element.to_affine_coords(&self.group, &self.bn_ctx)?;
            Ok(Some(x.to_be_vec(length)))
        }
    }

    // IEEE Std 802.11-2016 12.4.7.2.4
    fn element_to_octets(&self, element: &Self::Element) -> Result<Vec<u8>, Error> {
        let length = self.group.get_degree().div_ceil(8);
        let (x, y) = element.to_affine_coords(&self.group, &self.bn_ctx)?;
        let mut res = x.to_be_vec(length);
        res.append(&mut y.to_be_vec(length));
        Ok(res)
    }

    fn element_to_octets_compact(&self, element: &Self::Element) -> Result<Vec<u8>, Error> {
        let length = self.group.get_degree().div_ceil(8);
        let x = element.to_affine_coords_x(&self.group, &self.bn_ctx)?;
        let res = x.to_be_vec(length);
        Ok(res)
    }

    // IEEE Std 802.11-2016 12.4.7.2.5
    fn element_from_octets(&self, octets: &[u8]) -> Result<Option<Self::Element>, Error> {
        let length = self.group.get_degree().div_ceil(8);
        if octets.len() != length * 2 {
            warn!("element_from_octets called with wrong number of octets");
            return Ok(None);
        }
        let x = Bignum::new_from_slice(&octets[0..length])?;
        let y = Bignum::new_from_slice(&octets[length..])?;
        Ok(EcPoint::new_from_affine_coords(x, y, &self.group, &self.bn_ctx).ok())
    }

    fn element_from_octets_compact(&self, octets: &[u8]) -> Result<Option<Self::Element>, Error> {
        let length = self.group.get_degree().div_ceil(8);
        if octets.len() != length {
            warn!("element_from_octets_compact called with wrong number of octets");
            return Ok(None);
        }
        let x = Bignum::new_from_slice(&octets)?;
        Ok(EcPoint::new_from_compressed_coords(x, &self.group, &self.bn_ctx).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hmac_utils::HmacUtilsImpl;
    use ieee80211::{MacAddr, Ssid};
    use mundane::hash::Sha256;
    use std::convert::TryFrom;
    use std::sync::LazyLock;

    // IEEE Std 802.11-2020 J.10
    // SAE test vectors (common)
    const TEST_GROUP: EcGroupId = EcGroupId::P256;
    const TEST_SSID: &'static str = "byteme";
    const TEST_PWD: &'static str = "mekmitasdigoat";
    const TEST_PWD_ID: &'static str = "psk4internet";

    // IEEE Std 802.11-2020 J.10
    // Test vectors for looping PWE generation
    static TEST_LOOP_STA_A: LazyLock<MacAddr> =
        LazyLock::new(|| MacAddr::from([0x4d, 0x3f, 0x2f, 0xff, 0xe3, 0x87]));
    static TEST_LOOP_STA_B: LazyLock<MacAddr> =
        LazyLock::new(|| MacAddr::from([0xa5, 0xd8, 0xaa, 0x95, 0x8e, 0x3c]));
    const TEST_LOOP_PWE_X: &'static str =
        "da6eb7b06a1ac5624974f90afdd6a8e9d5722634cf987c34defc91a9874e5658";
    const TEST_LOOP_PWE_Y: &'static str =
        "f4fefd130bd5be08fe68af3e4a290272ec065fd3671f3c25bf8ec419ddc9b822";

    // IEEE Std 802.11-2020 J.10
    // Test vectors for direct PWE generation
    static TEST_DIRECT_STA_A: LazyLock<MacAddr> =
        LazyLock::new(|| MacAddr::from([0x00, 0x09, 0x5b, 0x66, 0xec, 0x1e]));
    static TEST_DIRECT_STA_B: LazyLock<MacAddr> =
        LazyLock::new(|| MacAddr::from([0x00, 0x0b, 0x6b, 0xd9, 0x02, 0x46]));

    const TEST_DIRECT_PT_X: &'static str =
        "b6e38c98750c684b5d17c3d8c9a4100b39931279187ca6cced5f37ef46ddfa97";
    const TEST_DIRECT_PT_Y: &'static str =
        "5687e972e50f73e3898861e7edad21bea7d5f622df88243bb804920ae8e647fa";
    const TEST_DIRECT_PWE_X: &'static str =
        "c93049b9e64000f848201649e999f2b5c22dea69b5632c9df4d633b8aa1f6c1e";
    const TEST_DIRECT_PWE_Y: &'static str =
        "73634e94b53d82e7383a8d258199d9dc1a5ee8269d060382ccbf33e614ff59a0";

    fn make_group() -> Group {
        let group = boringssl::EcGroup::new(TEST_GROUP).unwrap();
        let bn_ctx = boringssl::BignumCtx::new().unwrap();
        Group { id: TEST_GROUP, group, bn_ctx }
    }

    fn bn(value: u64) -> Bignum {
        Bignum::new_from_u64(value).unwrap()
    }

    #[test]
    fn get_group_id() {
        let group = make_group();
        assert_eq!(group.group_id(), TEST_GROUP.to_u16().unwrap());
    }

    #[test]
    fn generate_pwe_loop() {
        let group = make_group();
        let length = group.group.get_degree().div_ceil(8);
        let params = SaeParameters {
            hmac: Box::new(HmacUtilsImpl::<Sha256>::new()),
            pwe_method: PweMethod::Loop,
            ssid: Ssid::try_from(TEST_SSID).unwrap(),
            password: Vec::from(TEST_PWD),
            password_id: None,
            sta_a_mac: *TEST_LOOP_STA_A,
            sta_b_mac: *TEST_LOOP_STA_B,
        };
        let pwe = group.generate_pwe(&params).unwrap();
        let (x, y) = pwe.to_affine_coords(&group.group, &group.bn_ctx).unwrap();
        assert_eq!(x.to_be_vec(length), hex::decode(TEST_LOOP_PWE_X).unwrap());
        assert_eq!(y.to_be_vec(length), hex::decode(TEST_LOOP_PWE_Y).unwrap());

        // The PWE should not change depending on the order of mac addresses.
        let params =
            SaeParameters { sta_a_mac: *TEST_LOOP_STA_B, sta_b_mac: *TEST_LOOP_STA_A, ..params };
        let pwe = group.generate_pwe(&params).unwrap();
        let (x, y) = pwe.to_affine_coords(&group.group, &group.bn_ctx).unwrap();
        assert_eq!(x.to_be_vec(length), hex::decode(TEST_LOOP_PWE_X).unwrap());
        assert_eq!(y.to_be_vec(length), hex::decode(TEST_LOOP_PWE_Y).unwrap());
    }

    #[test]
    fn generate_pwe_direct() {
        let group = make_group();
        let length = group.group.get_degree().div_ceil(8);
        let params = SaeParameters {
            hmac: Box::new(HmacUtilsImpl::<Sha256>::new()),
            pwe_method: PweMethod::Direct,
            ssid: Ssid::try_from(TEST_SSID).unwrap(),
            password: Vec::from(TEST_PWD),
            password_id: Some(Vec::from(TEST_PWD_ID)),
            sta_a_mac: *TEST_DIRECT_STA_A,
            sta_b_mac: *TEST_DIRECT_STA_B,
        };
        let pwe = group.generate_pwe(&params).unwrap();
        let (x, y) = pwe.to_affine_coords(&group.group, &group.bn_ctx).unwrap();
        assert_eq!(x.to_be_vec(length), hex::decode(TEST_DIRECT_PWE_X).unwrap());
        assert_eq!(y.to_be_vec(length), hex::decode(TEST_DIRECT_PWE_Y).unwrap());

        // The PWE should not change depending on the order of mac addresses.
        let params = SaeParameters {
            sta_a_mac: *TEST_DIRECT_STA_B,
            sta_b_mac: *TEST_DIRECT_STA_A,
            ..params
        };
        let pwe = group.generate_pwe(&params).unwrap();
        let (x, y) = pwe.to_affine_coords(&group.group, &group.bn_ctx).unwrap();
        assert_eq!(x.to_be_vec(length), hex::decode(TEST_DIRECT_PWE_X).unwrap());
        assert_eq!(y.to_be_vec(length), hex::decode(TEST_DIRECT_PWE_Y).unwrap());
    }

    #[test]
    fn generate_pwe_loop_no_pwd_id() {
        let group = make_group();
        let params = SaeParameters {
            hmac: Box::new(HmacUtilsImpl::<Sha256>::new()),
            pwe_method: PweMethod::Loop,
            ssid: Ssid::try_from(TEST_SSID).unwrap(),
            password: Vec::from(TEST_PWD),
            password_id: Some(Vec::from(TEST_PWD_ID)),
            sta_a_mac: *TEST_LOOP_STA_A,
            sta_b_mac: *TEST_LOOP_STA_B,
        };
        let pwe = group.generate_pwe(&params);
        // IEEE Std 802.11-2020: password ID cannot be used with PWE generation by looping
        assert!(pwe.is_err());
    }

    #[test]
    fn generate_pt() {
        let group = make_group();
        let params = SaeParameters {
            hmac: Box::new(HmacUtilsImpl::<Sha256>::new()),
            pwe_method: PweMethod::Direct,
            ssid: Ssid::try_from(TEST_SSID).unwrap(),
            password: Vec::from(TEST_PWD),
            password_id: Some(Vec::from(TEST_PWD_ID)),
            sta_a_mac: *TEST_DIRECT_STA_A,
            sta_b_mac: *TEST_DIRECT_STA_B,
        };

        let pt = group.generate_pt(&params).unwrap();
        let (pt_x, pt_y) = pt.to_affine_coords(&group.group, &group.bn_ctx).unwrap();
        assert_eq!(hex::encode(pt_x.to_be_vec(0)), TEST_DIRECT_PT_X);
        assert_eq!(hex::encode(pt_y.to_be_vec(0)), TEST_DIRECT_PT_Y);
    }

    #[test]
    fn test_element_to_octets() {
        let x = Bignum::new_from_slice(&hex::decode(TEST_LOOP_PWE_X).unwrap()).unwrap();
        let y = Bignum::new_from_slice(&hex::decode(TEST_LOOP_PWE_Y).unwrap()).unwrap();
        let group = make_group();
        let element = EcPoint::new_from_affine_coords(x, y, &group.group, &group.bn_ctx).unwrap();

        let octets = group.element_to_octets(&element).unwrap();
        let mut expected = hex::decode(TEST_LOOP_PWE_X).unwrap();
        expected.extend_from_slice(&hex::decode(TEST_LOOP_PWE_Y).unwrap());
        assert_eq!(octets, expected);
    }

    #[test]
    fn test_element_to_octets_padding() {
        let group = make_group();
        // We compute a point on the curve with a short x coordinate -- the
        // generated octets should still be 64 in length, zero padded.
        let x = bn(0xffffffff);
        let element = EcPoint::new_from_compressed_coords(x, &group.group, &group.bn_ctx).unwrap();

        let octets = group.element_to_octets(&element).unwrap();
        let mut expected_x = vec![0x00; 28];
        expected_x.extend_from_slice(&[0xff; 4]);
        assert_eq!(octets.len(), 64);
        assert_eq!(&octets[0..32], &expected_x[0..32]);
    }

    #[test]
    fn test_element_from_octets() {
        let mut octets = hex::decode(TEST_LOOP_PWE_X).unwrap();
        octets.extend_from_slice(&hex::decode(TEST_LOOP_PWE_Y).unwrap());
        let group = make_group();
        let element = group.element_from_octets(&octets).unwrap();
        assert!(element.is_some());
        let element = element.unwrap();

        let expected_x = Bignum::new_from_slice(&hex::decode(TEST_LOOP_PWE_X).unwrap()).unwrap();
        let expected_y = Bignum::new_from_slice(&hex::decode(TEST_LOOP_PWE_Y).unwrap()).unwrap();
        let (x, y) = element.to_affine_coords(&group.group, &group.bn_ctx).unwrap();

        assert_eq!(x, expected_x);
        assert_eq!(y, expected_y);
    }

    #[test]
    fn test_element_from_octets_padded() {
        let mut octets = hex::decode(TEST_LOOP_PWE_X).unwrap();
        octets.extend_from_slice(&hex::decode(TEST_LOOP_PWE_Y).unwrap());
        octets.extend_from_slice(&[0xff; 10]);
        let group = make_group();
        let element = group.element_from_octets(&octets).unwrap();
        assert!(element.is_none());
    }

    #[test]
    fn test_element_from_octets_truncated() {
        let mut octets = hex::decode(TEST_LOOP_PWE_X).unwrap();
        octets.extend_from_slice(&hex::decode(TEST_LOOP_PWE_Y).unwrap());
        octets.truncate(octets.len() - 10);
        let group = make_group();
        let element = group.element_from_octets(&octets).unwrap();
        assert!(element.is_none());
    }

    #[test]
    fn test_element_from_octets_bad_point() {
        let mut octets = hex::decode(TEST_LOOP_PWE_X).unwrap();
        octets.extend_from_slice(&hex::decode(TEST_LOOP_PWE_Y).unwrap());
        let idx = octets.len() - 1;
        octets[idx] += 1; // This is no longer the right Y value for this X.
        let group = make_group();
        let element = group.element_from_octets(&octets).unwrap();
        assert!(element.is_none());
    }
}
