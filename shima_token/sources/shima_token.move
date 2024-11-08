module shima_token::shim {
    use sui::coin::{Self, TreasuryCap};
    use sui::table::{Self, Table};
    use shima_token::icon::{get_icon_url};

    // Define the struct for SHIM token with visibility annotation.
    public struct SHIM has drop {}

    // Add a struct to keep track of the total supply
    public struct Token has key, store {
        id: UID,
        total_supply: u64,
        max_supply: u64,
    }

    // Internal init function to initialize the SHIM token.
    fun init(witness: SHIM, ctx: &mut TxContext) {
        let (treasury_cap, metadata) = coin::create_currency<SHIM>(
            witness, 9, 
            b"SHIM",
            b"Shima",
            b"A Community-Driven Token Soaring on the SUI Blockchain",
            option::some(get_icon_url()), // icon_url
            ctx);

        transfer::public_freeze_object(metadata);
        transfer::public_transfer(treasury_cap, tx_context::sender(ctx));

        // Initialize the total supply with a maximum limit
        let total_supply = 20_000_000_000;
        let max_supply = 20_000_000_000;
        let token = Token { id: object::new(ctx), total_supply, max_supply };
        transfer::public_transfer(token, tx_context::sender(ctx));
    }

    public fun mint(
        treasury_cap: &mut TreasuryCap<SHIM>, 
        amount: u64, 
        recipient: address, 
        ctx: &mut TxContext,
    ) {
        // Minting logic is done through the treasury_cap
        let current_supply = coin::total_supply(treasury_cap);

        // Check if minting exceeds the max supply
        assert!(current_supply + amount <= 20_000_000_000, 0);

        let coin = coin::mint(treasury_cap, amount, ctx);
        transfer::public_transfer(coin, recipient);
    }
}
