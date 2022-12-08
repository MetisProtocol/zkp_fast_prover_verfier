use itertools::Itertools;
use num_traits::{One, Zero};
use rayon::prelude::{IntoParallelIterator, IntoParallelRefIterator, ParallelIterator};
use std::error::Error;
use std::fmt;
use std::marker::PhantomData;

use super::b_field_element::BFieldElement;
use super::other::{log_2_ceil, log_2_floor};
use super::polynomial::Polynomial;
use super::traits::{CyclicGroupGenerator, ModPowU32};
use super::x_field_element::XFieldElement;
use crate::shared_math::ntt::{intt, ntt};
use crate::shared_math::traits::FiniteField;
use crate::util_types::algebraic_hasher::{AlgebraicHasher, Hashable};
use crate::util_types::merkle_tree::{MerkleTree, PartialAuthenticationPath};
use crate::util_types::proof_stream::ProofStream;

use super::rescue_prime_digest::Digest;

impl Error for ValidationError {}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Deserialization error for LowDegreeProof: {:?}", self)
    }
}

#[derive(PartialEq, Eq, Debug)]
pub enum ValidationError {
    BadMerkleProof,
    BadSizedProof,
    NonPostiveRoundCount,
    NotColinear(usize),
    LastIterationTooHighDegree,
    BadMerkleRootForLastCodeword,
}

#[derive(Debug, Clone)]
pub struct FriDomain {
    pub offset: BFieldElement,
    pub omega: BFieldElement,
    pub length: usize,
}

impl FriDomain {
    pub fn x_evaluate(&self, polynomial: &Polynomial<XFieldElement>) -> Vec<XFieldElement> {
        polynomial.fast_coset_evaluate(&self.offset, self.omega, self.length as usize)
    }

    pub fn x_interpolate(&self, values: &[XFieldElement]) -> Polynomial<XFieldElement> {
        Polynomial::<XFieldElement>::fast_coset_interpolate(&self.offset, self.omega, values)
    }

    pub fn b_domain_value(&self, index: u32) -> BFieldElement {
        self.omega.mod_pow_u32(index) * self.offset
    }

    pub fn b_domain_values(&self) -> Vec<BFieldElement> {
        (0..self.length)
            .map(|i| self.omega.mod_pow_u32(i as u32) * self.offset)
            .collect()
    }

    pub fn b_evaluate(&self, polynomial: &Polynomial<BFieldElement>) -> Vec<BFieldElement> {
        let zero = BFieldElement::zero();
        let mut polynomial_representation: Vec<BFieldElement> =
            polynomial.scale(&self.offset).coefficients;
        polynomial_representation.resize(self.length as usize, zero);
        ntt(
            &mut polynomial_representation,
            self.omega,
            log_2_ceil(self.length as u128) as u32,
        );

        polynomial_representation
    }

    pub fn b_interpolate(&self, values: &[BFieldElement]) -> Polynomial<BFieldElement> {
        Polynomial::<BFieldElement>::fast_coset_interpolate(&self.offset, self.omega, values)
    }
}

#[derive(Debug, Clone)]
pub struct Fri<H> {
    pub expansion_factor: usize,         // = domain_length / trace_length
    pub colinearity_checks_count: usize, // number of colinearity checks in each round
    pub domain: FriDomain,
    _hasher: PhantomData<H>,
}

type CodewordEvaluation<T> = (usize, T);

impl<H> Fri<H>
where
    H: AlgebraicHasher + Send + Sync,
{
    pub fn new(
        offset: BFieldElement,
        omega: BFieldElement,
        domain_length: usize,
        expansion_factor: usize,
        colinearity_checks_count: usize,
    ) -> Self {
        let domain = FriDomain {
            offset,
            omega,
            length: domain_length,
        };
        let _hasher = PhantomData;
        Self {
            domain,
            expansion_factor,
            colinearity_checks_count,
            _hasher,
        }
    }

    /// Build the (deduplicated) Merkle authentication paths for the codeword at the given indices
    /// and enqueue the corresponding values and (partial) authentication paths on the proof stream.
    fn enqueue_auth_pairs(
        indices: &[usize],
        codeword: &[XFieldElement],
        merkle_tree: &MerkleTree<H>,
        proof_stream: &mut ProofStream,
    ) {
        let value_ap_pairs: Vec<(PartialAuthenticationPath<Digest>, XFieldElement)> = merkle_tree
            .get_authentication_structure(indices)
            .into_iter()
            .zip(indices.iter())
            .map(|(ap, i)| (ap, codeword[*i]))
            .collect_vec();
        proof_stream
            .enqueue_length_prepended(&value_ap_pairs)
            .expect("Enqueuing must succeed")
    }

    /// Given a set of `indices`, a merkle `root`, and the (correctly set) `proof_stream`, verify
    /// whether the values at the `indices` are members of the set committed to by the merkle `root`
    /// and return these values if they are. Fails otherwise.
    fn dequeue_and_authenticate(
        indices: &[usize],
        root: Digest,
        proof_stream: &mut ProofStream,
    ) -> Result<Vec<XFieldElement>, Box<dyn Error>> {
        let (paths, values): (Vec<PartialAuthenticationPath<Digest>>, Vec<XFieldElement>) = proof_stream
            .dequeue_length_prepended::<Vec<(PartialAuthenticationPath<Digest>, XFieldElement)>>()?
            .into_iter()
            .unzip();
        let digests: Vec<Digest> = values
            .par_iter()
            .map(|v| H::hash_slice(&v.to_sequence()))
            .collect();
        let path_digest_pairs = paths.into_iter().zip(digests).collect_vec();

        if MerkleTree::<H>::verify_authentication_structure(root, indices, &path_digest_pairs) {
            Ok(values)
        } else {
            Err(Box::new(ValidationError::BadMerkleProof))
        }
    }

    pub fn prove(
        &self,
        codeword: &[XFieldElement],
        proof_stream: &mut ProofStream,
    ) -> Result<Vec<usize>, Box<dyn Error>> {
        assert_eq!(
            self.domain.length,
            codeword.len(),
            "Initial codeword length must match that set in FRI object"
        );

        // Commit phase
        let (codewords, merkle_trees): (Vec<Vec<XFieldElement>>, Vec<MerkleTree<H>>) =
            self.commit(codeword, proof_stream)?.into_iter().unzip();

        // fiat-shamir phase (get indices)
        let top_level_indices = self.sample_indices(&proof_stream.prover_fiat_shamir());

        // query phase
        let initial_a_indices: Vec<usize> = top_level_indices.clone();
        Self::enqueue_auth_pairs(&initial_a_indices, codeword, &merkle_trees[0], proof_stream);
        let mut current_domain_len = self.domain.length;
        let mut b_indices: Vec<usize> = initial_a_indices;

        for r in 0..merkle_trees.len() - 1 {
            debug_assert_eq!(
                codewords[r].len(),
                current_domain_len,
                "The current domain length needs to be the same as the length of the \
                current codeword"
            );
            b_indices = b_indices
                .iter()
                .map(|x| (x + current_domain_len / 2) % current_domain_len)
                .collect();
            Self::enqueue_auth_pairs(&b_indices, &codewords[r], &merkle_trees[r], proof_stream);
            current_domain_len /= 2;
        }

        Ok(top_level_indices)
    }

    #[allow(clippy::type_complexity)]
    fn commit(
        &self,
        codeword: &[XFieldElement],
        proof_stream: &mut ProofStream,
    ) -> Result<Vec<(Vec<XFieldElement>, MerkleTree<H>)>, Box<dyn Error>> {
        let mut generator = self.domain.omega;
        let mut offset = self.domain.offset;
        let mut codeword_local = codeword.to_vec();

        let one: XFieldElement = XFieldElement::one();
        let two: XFieldElement = one + one;
        let two_inv = one / two;

        // Compute and send Merkle root
        let mut digests: Vec<Digest> = codeword_local
            .par_iter()
            .map(|x| H::hash_slice(&x.to_sequence()))
            .collect();
        let mut mt = MerkleTree::from_digests(&digests);
        proof_stream.enqueue(&mt.get_root())?;
        let mut values_and_merkle_trees = vec![(codeword_local.clone(), mt)];

        let (num_rounds, _) = self.num_rounds();
        for _ in 0..num_rounds {
            let n = codeword_local.len();

            // Sanity check to verify that generator has the right order; requires ModPowU64
            //assert!(generator.inv() == generator.mod_pow((n - 1).into())); // TODO: REMOVE

            // Get challenge, one just acts as *any* element in this field -- the field element
            // is completely determined from the byte stream.
            let challenge: Digest = proof_stream.prover_fiat_shamir();
            let alpha: XFieldElement = XFieldElement::sample(&challenge);

            let x_offset: Vec<BFieldElement> = generator
                .get_cyclic_group_elements(None)
                .into_par_iter()
                .map(|x| x * offset)
                .collect();

            let x_offset_inverses = BFieldElement::batch_inversion(x_offset);
            codeword_local = (0..n / 2)
                .into_par_iter()
                .map(|i| {
                    two_inv
                        * ((one + alpha * x_offset_inverses[i]) * codeword_local[i]
                            + (one - alpha * x_offset_inverses[i]) * codeword_local[n / 2 + i])
                })
                .collect();

            // Compute and send Merkle root
            digests = codeword_local
                .par_iter()
                .map(|x| H::hash_slice(&x.to_sequence()))
                .collect();
            mt = MerkleTree::from_digests(&digests);
            proof_stream.enqueue(&mt.get_root())?;
            values_and_merkle_trees.push((codeword_local.clone(), mt));

            // Update subgroup generator and offset
            generator = generator * generator;
            offset = offset * offset;
        }

        // Send the last codeword
        let last_codeword = codeword_local;
        proof_stream.enqueue_length_prepended(&last_codeword)?;

        Ok(values_and_merkle_trees)
    }

    // Return the c-indices for the 1st round of FRI
    fn sample_indices(&self, seed: &Digest) -> Vec<usize> {
        // This algorithm starts with the inner-most indices to pick up
        // to `last_codeword_length` indices from the codeword in the last round.
        // It then calculates the indices in the subsequent rounds by choosing
        // between the two possible next indices in the next round until we get
        // the c-indices for the first round.
        let num_rounds = self.num_rounds().0;
        let last_codeword_length = self.domain.length >> num_rounds;
        assert!(
            self.colinearity_checks_count <= last_codeword_length,
            "Requested number of indices must not exceed length of last codeword"
        );

        let mut last_indices: Vec<usize> = vec![];
        let mut remaining_last_round_exponents: Vec<usize> = (0..last_codeword_length).collect();
        let mut counter = 0u32;
        for _ in 0..self.colinearity_checks_count {
            let mut seed_local = seed.to_sequence();
            seed_local.append(&mut counter.to_sequence());
            let hash = H::hash_slice(&seed_local);
            let index: usize =
                H::sample_index_not_power_of_two(&hash, remaining_last_round_exponents.len());
            last_indices.push(remaining_last_round_exponents.remove(index));
            counter += 1;
        }

        // Use last indices to derive first c-indices
        let mut indices = last_indices;
        for i in 1..num_rounds {
            let codeword_length = last_codeword_length << i;

            let mut new_indices: Vec<usize> = vec![];
            for index in indices {
                let mut seed_local = seed.to_sequence();
                seed_local.append(&mut counter.to_sequence());
                let hash = H::hash_slice(&seed_local);
                let reduce_modulo: bool = H::sample_index(&hash, 2) == 0;
                let new_index = if reduce_modulo {
                    index + codeword_length / 2
                } else {
                    index
                };
                new_indices.push(new_index);

                counter += 1;
            }

            indices = new_indices;
        }

        indices
    }

    pub fn verify(
        &self,
        proof_stream: &mut ProofStream,
    ) -> Result<Vec<CodewordEvaluation<XFieldElement>>, Box<dyn Error>> {
        let mut omega = self.domain.omega;
        let mut offset = self.domain.offset;
        let (num_rounds, degree_of_last_round) = self.num_rounds();

        // Extract all roots and calculate alpha, the challenges
        let mut roots: Vec<Digest> = vec![];
        let mut alphas: Vec<XFieldElement> = vec![];
        let first_root: Digest = proof_stream.dequeue(Digest::BYTES)?;
        roots.push(first_root);

        for _ in 0..num_rounds {
            // Get a challenge from the proof stream
            let challenge: Digest = proof_stream.verifier_fiat_shamir();
            let alpha: XFieldElement = XFieldElement::sample(&challenge);
            alphas.push(alpha);
            roots.push(proof_stream.dequeue(Digest::BYTES)?);
        }

        // Extract last codeword
        let mut last_codeword: Vec<XFieldElement> =
            proof_stream.dequeue_length_prepended::<Vec<XFieldElement>>()?;

        // Check if last codeword matches the given root
        let leaves: Vec<_> = last_codeword
            .iter()
            .map(|x| H::hash_slice(&x.to_sequence()))
            .collect();
        let last_codeword_mt = MerkleTree::<H>::from_digests(&leaves);
        let last_root = roots.last().unwrap();
        if *last_root != last_codeword_mt.get_root() {
            return Err(Box::new(ValidationError::BadMerkleRootForLastCodeword));
        }

        // Verify that last codeword is of sufficiently low degree
        let mut last_omega = omega;
        let mut last_offset = offset;
        for _ in 0..num_rounds {
            last_omega = last_omega * last_omega;
            last_offset = last_offset * last_offset;
        }

        // Compute interpolant to get the degree of the last codeword
        // Note that we don't have to scale the polynomial back to the
        // trace subgroup since we only check its degree and don't use
        // it further.
        let log_2_of_n = log_2_floor(last_codeword.len() as u128) as u32;
        intt::<XFieldElement>(&mut last_codeword, last_omega, log_2_of_n);
        let last_poly_degree: isize = (Polynomial::<XFieldElement> {
            coefficients: last_codeword,
        })
        .degree();
        if last_poly_degree > degree_of_last_round as isize {
            return Err(Box::new(ValidationError::LastIterationTooHighDegree));
        }

        let mut a_indices: Vec<usize> = self.sample_indices(&proof_stream.verifier_fiat_shamir());

        // for every round, check consistency of subsequent layers
        let mut codeword_evaluations: Vec<CodewordEvaluation<XFieldElement>> = vec![];
        let mut a_values = Self::dequeue_and_authenticate(&a_indices, roots[0], proof_stream)?;

        // set up "B" for offsetting inside loop.  Note that "B" and "A" indices
        // can be calcuated from each other.
        let mut b_indices = a_indices.clone();
        let mut current_domain_len = self.domain.length;

        for r in 0..num_rounds as usize {
            // get "B" indices and verify set membership of corresponding values
            b_indices = b_indices
                .iter()
                .map(|x| (x + current_domain_len / 2) % current_domain_len)
                .collect();

            let b_values = Self::dequeue_and_authenticate(&b_indices, roots[r], proof_stream)?;

            debug_assert_eq!(
                self.colinearity_checks_count,
                a_indices.len(),
                "There must be equally many 'a indices' as there are colinearity checks."
            );
            debug_assert_eq!(
                self.colinearity_checks_count,
                b_indices.len(),
                "There must be equally many 'b indices' as there are colinearity checks."
            );
            debug_assert_eq!(
                self.colinearity_checks_count,
                a_values.len(),
                "There must be equally many 'a values' as there are colinearity checks."
            );
            debug_assert_eq!(
                self.colinearity_checks_count,
                b_values.len(),
                "There must be equally many 'b values' as there are colinearity checks."
            );

            // compute "C" indices and values for next round from "A" and "B`"" of current round
            current_domain_len /= 2;
            let c_indices = a_indices.iter().map(|x| x % current_domain_len).collect();
            let c_values = (0..self.colinearity_checks_count)
                .into_par_iter()
                .map(|i| {
                    Polynomial::<XFieldElement>::get_colinear_y(
                        (
                            self.get_evaluation_argument(a_indices[i], r).lift(),
                            a_values[i],
                        ),
                        (
                            self.get_evaluation_argument(b_indices[i], r).lift(),
                            b_values[i],
                        ),
                        alphas[r],
                    )
                })
                .collect();

            // Return top-level values to caller
            if r == 0 {
                for s in 0..self.colinearity_checks_count {
                    codeword_evaluations.push((a_indices[s], a_values[s]));
                    codeword_evaluations.push((b_indices[s], b_values[s]));
                }
            }

            // Notice that next rounds "A"s correspond to current rounds "C":
            a_indices = c_indices;
            a_values = c_values;

            // Update subgroup generator and offset
            omega = omega * omega;
            offset = offset * offset;
        }

        Ok(codeword_evaluations)
    }

    fn get_evaluation_argument(&self, idx: usize, round: usize) -> BFieldElement {
        (self.domain.offset * self.domain.omega.mod_pow_u32(idx as u32))
            .mod_pow_u32(2u32.pow(round as u32))
    }

    pub fn get_evaluation_domain(&self) -> Vec<BFieldElement> {
        let omega_domain = self.domain.omega.get_cyclic_group_elements(None);
        omega_domain
            .into_iter()
            .map(|x| x * self.domain.offset)
            .collect()
    }

    fn num_rounds(&self) -> (u8, u32) {
        let max_degree = (self.domain.length / self.expansion_factor) - 1;
        let mut rounds_count = log_2_ceil(max_degree as u128 + 1) as u8;
        let mut max_degree_of_last_round = 0u32;
        if self.expansion_factor < self.colinearity_checks_count {
            let num_missed_rounds = log_2_ceil(
                (self.colinearity_checks_count as f64 / self.expansion_factor as f64).ceil()
                    as u128,
            ) as u8;
            rounds_count -= num_missed_rounds;
            max_degree_of_last_round = 2u32.pow(num_missed_rounds as u32) - 1;
        }

        (rounds_count, max_degree_of_last_round)
    }
}

#[cfg(test)]
mod fri_domain_tests {
    use num_traits::One;

    use super::*;
    use crate::shared_math::b_field_element::BFieldElement;
    use crate::shared_math::traits::PrimitiveRootOfUnity;
    use crate::shared_math::x_field_element::XFieldElement;

    #[test]
    fn x_values_test() {
        // pol = x^3
        let x_squared_coefficients = vec![
            BFieldElement::zero(),
            BFieldElement::zero(),
            BFieldElement::zero(),
            BFieldElement::one(),
        ];

        for order in [4, 8, 32] {
            let omega = BFieldElement::primitive_root_of_unity(order).unwrap();
            let domain = FriDomain {
                offset: BFieldElement::generator(),
                omega,
                length: order as usize,
            };
            let expected_x_values: Vec<BFieldElement> = (0..order)
                .map(|i| BFieldElement::generator() * omega.mod_pow(i as u64))
                .collect();
            let x_values = domain.b_domain_values();
            assert_eq!(expected_x_values, x_values);

            // Verify that `x_value` also returns expected values
            for i in 0..order {
                assert_eq!(
                    expected_x_values[i as usize],
                    domain.b_domain_value(i as u32)
                );
            }

            let pol = Polynomial::<BFieldElement>::new(x_squared_coefficients.clone());
            let values = domain.b_evaluate(&pol);
            assert_ne!(values, x_squared_coefficients);
            let interpolant = domain.b_interpolate(&values);
            assert_eq!(pol, interpolant);

            // Verify that batch-evaluated values match a manual evaluation
            for i in 0..order {
                assert_eq!(
                    pol.evaluate(&domain.b_domain_value(i as u32)),
                    values[i as usize]
                );
            }

            let x_squared_coefficients_lifted: Vec<XFieldElement> = x_squared_coefficients
                .clone()
                .into_iter()
                .map(|x| x.lift())
                .collect();
            let xpol = Polynomial::new(x_squared_coefficients_lifted.clone());
            let x_field_x_values = domain.x_evaluate(&xpol);
            assert_ne!(x_field_x_values, x_squared_coefficients_lifted);
            let x_interpolant = domain.x_interpolate(&x_field_x_values);
            assert_eq!(xpol, x_interpolant);
        }
    }
}

#[cfg(test)]
mod fri_tests {
    use super::*;
    use crate::shared_math::b_field_element::BFieldElement;
    use crate::shared_math::rescue_prime_regular::RescuePrimeRegular;
    use crate::shared_math::traits::PrimitiveRootOfUnity;
    use crate::shared_math::traits::{CyclicGroupGenerator, ModPowU32};
    use crate::shared_math::x_field_element::XFieldElement;
    use itertools::Itertools;

    #[test]
    fn get_rounds_count_test() {
        type H = blake3::Hasher;

        let subgroup_order = 512;
        let expansion_factor = 4;
        let mut fri: Fri<H> = get_x_field_fri_test_object::<H>(subgroup_order, expansion_factor, 2);

        assert_eq!((7, 0), fri.num_rounds());
        fri.colinearity_checks_count = 8;
        assert_eq!((6, 1), fri.num_rounds());
        fri.colinearity_checks_count = 10;
        assert_eq!((5, 3), fri.num_rounds());
        fri.colinearity_checks_count = 16;
        assert_eq!((5, 3), fri.num_rounds());
        fri.colinearity_checks_count = 17;
        assert_eq!((4, 7), fri.num_rounds());
        fri.colinearity_checks_count = 18;
        assert_eq!((4, 7), fri.num_rounds());
        fri.colinearity_checks_count = 31;
        assert_eq!((4, 7), fri.num_rounds());
        fri.colinearity_checks_count = 32;
        assert_eq!((4, 7), fri.num_rounds());
        fri.colinearity_checks_count = 33;
        assert_eq!((3, 15), fri.num_rounds());

        fri.domain.length = 256;
        assert_eq!((2, 15), fri.num_rounds());
        fri.colinearity_checks_count = 32;
        assert_eq!((3, 7), fri.num_rounds());

        fri.colinearity_checks_count = 32;
        fri.domain.length = 1048576;
        fri.expansion_factor = 8;
        assert_eq!((15, 3), fri.num_rounds());

        fri.colinearity_checks_count = 33;
        fri.domain.length = 1048576;
        fri.expansion_factor = 8;
        assert_eq!((14, 7), fri.num_rounds());

        fri.colinearity_checks_count = 63;
        fri.domain.length = 1048576;
        fri.expansion_factor = 8;
        assert_eq!((14, 7), fri.num_rounds());

        fri.colinearity_checks_count = 64;
        fri.domain.length = 1048576;
        fri.expansion_factor = 8;
        assert_eq!((14, 7), fri.num_rounds());

        fri.colinearity_checks_count = 65;
        fri.domain.length = 1048576;
        fri.expansion_factor = 8;
        assert_eq!((13, 15), fri.num_rounds());

        fri.domain.length = 256;
        fri.expansion_factor = 4;
        fri.colinearity_checks_count = 17;
        assert_eq!((3, 7), fri.num_rounds());
    }

    #[test]
    fn fri_on_x_field_test() {
        type Hasher = RescuePrimeRegular;

        let subgroup_order = 1024;
        let expansion_factor = 4;
        let colinearity_check_count = 6;
        let fri: Fri<Hasher> =
            get_x_field_fri_test_object(subgroup_order, expansion_factor, colinearity_check_count);
        let mut proof_stream: ProofStream = ProofStream::default();
        let subgroup = fri.domain.omega.lift().get_cyclic_group_elements(None);

        let ret = fri.prove(&subgroup, &mut proof_stream).unwrap();
        assert_eq!(fri.colinearity_checks_count, ret.len());
        assert_eq!(colinearity_check_count, ret.len());
        let verify_result = fri.verify(&mut proof_stream);
        assert!(verify_result.is_ok());
    }

    #[test]
    fn fri_x_field_limit_test() {
        type Hasher = blake3::Hasher;

        let subgroup_order = 1024;
        let expansion_factor = 4;
        let colinearity_check_count = 6;
        let fri: Fri<Hasher> =
            get_x_field_fri_test_object(subgroup_order, expansion_factor, colinearity_check_count);
        let subgroup = fri.domain.omega.get_cyclic_group_elements(None);

        let mut points: Vec<XFieldElement>;
        for n in &[1, 10, 50, 100, 255] {
            points = subgroup
                .clone()
                .iter()
                .map(|p| p.mod_pow_u32(*n).lift())
                .collect();

            // TODO: Test elsewhere that proof_stream can be re-used for multiple .prove().
            let mut proof_stream: ProofStream = ProofStream::default();
            let ret = fri.prove(&points, &mut proof_stream).unwrap();
            assert_eq!(colinearity_check_count, ret.len());

            let verify_result = fri.verify(&mut proof_stream);
            if verify_result.is_err() {
                println!(
                    "There are {} points, |<1024>^{}| = {}, and verify_result = {:?}",
                    points.len(),
                    n,
                    points.iter().unique().count(),
                    verify_result
                );
            }

            assert!(verify_result.is_ok());
        }

        // Negative test
        let too_high = subgroup_order as u32 / expansion_factor as u32;
        points = subgroup
            .iter()
            .map(|p| p.mod_pow_u32(too_high).lift())
            .collect();
        let mut proof_stream: ProofStream = ProofStream::default();
        fri.prove(&points, &mut proof_stream).unwrap();
        let verify_result = fri.verify(&mut proof_stream);
        assert!(verify_result.is_err());
    }

    fn get_x_field_fri_test_object<H>(
        subgroup_order: u64,
        expansion_factor: usize,
        colinearity_checks: usize,
    ) -> Fri<H>
    where
        H: AlgebraicHasher + Sized,
    {
        let maybe_omega = BFieldElement::primitive_root_of_unity(subgroup_order);

        // The following offset was picked arbitrarily by copying the one found in
        // `get_b_field_fri_test_object`. It does not generate the full Z_p\{0}, but
        // we're not sure it needs to, Alan?
        let offset: Option<BFieldElement> = Some(BFieldElement::new(7));

        let fri: Fri<H> = Fri::new(
            offset.unwrap(),
            maybe_omega.unwrap(),
            subgroup_order as usize,
            expansion_factor,
            colinearity_checks,
        );
        fri
    }
}
