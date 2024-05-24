// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use super::*;
use aptos_schemadb::{schema::fuzzing::assert_encode_decode, test_no_panic_decoding};
use proptest::prelude::*;

proptest! {
    #[test]
    fn test_encode_decode(
        stale_state_value_index_with_hash in any::<StaleStateValueWithKeyHashIndex>(),
    ) {
        assert_encode_decode::<StaleStateValueIndexWithKeyHashSchema>(&stale_state_value_index_with_hash, &());
    }
}

test_no_panic_decoding!(StaleStateValueIndexWithKeyHashSchema);
