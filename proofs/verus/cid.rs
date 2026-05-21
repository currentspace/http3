use vstd::prelude::*;

verus! {

spec fn valid_config_rotation(config_rotation: nat) -> bool {
    config_rotation <= 6nat
}

spec fn encoded_config_rotation(first_octet: nat) -> nat {
    first_octet / 32nat
}

spec fn random_low_bits(first_octet: nat) -> nat {
    first_octet % 32nat
}

proof fn plaintext_encoding_preserves_low_bits_and_rotation(
    original_first_octet: nat,
    config_rotation: nat,
)
    requires
        original_first_octet < 256nat,
        valid_config_rotation(config_rotation),
    ensures
        random_low_bits(config_rotation * 32nat + random_low_bits(original_first_octet))
            == random_low_bits(original_first_octet),
        encoded_config_rotation(config_rotation * 32nat + random_low_bits(original_first_octet))
            == config_rotation,
{
}

} // verus!

fn main() {}
