//! # DEX pallet
//!
//! ## Overview
//!
//! This pallet re-implements Uniswap V1 protocol for decentralized exchange of fungible assets.
//! Please refer to the [protocol description](https://docs.uniswap.org/protocol/V1/introduction)
//! and [smart contracts](https://github.com/Uniswap/v1-contracts) for more details.
//! DEX pallet allows users to create exchanges (i.e. liquidity pools), supply them with liquidity
//! (i.e. currency & assets), and perform trades (currency-to-asset, asset-to-currency, asset-to-asset).
//! DEX pallet also allows querying asset prices by custom RPC methods.
//!

#![cfg_attr(not(feature = "std"), no_std)]

use frame_support::traits::Currency;
use sp_std::prelude::*;

pub use pallet::*;
// pub use weights::WeightInfo;

type AccountIdOf<T> = <T as frame_system::Config>::AccountId;
type BalanceOf<T> = <<T as Config>::Currency as Currency<AccountIdOf<T>>>::Balance;
type AssetIdOf<T> = <T as Config>::AssetId;
type AssetBalanceOf<T> = <T as Config>::AssetBalance;

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use codec::EncodeLike;
	use frame_support::{
		pallet_prelude::*,
		sp_runtime::{
			traits::{
				AccountIdConversion, CheckedAdd, CheckedMul, CheckedSub, Convert, One, Saturating,
				Zero,
			},
			FixedPointNumber, FixedPointOperand, FixedU128,
		},
		storage::transactional,
		traits::{
			fungibles::{Create, Destroy, Inspect, Mutate, Transfer},
			tokens::{Balance, WithdrawConsequence},
			ExistenceRequirement,
		},
		transactional, PalletId,
	};
	use frame_system::{
		ensure_signed,
		pallet_prelude::{OriginFor, *},
		Origin,
	};
	use sp_std::fmt::Debug;

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T>(_);

	/// Configure the pallet by specifying the parameters and types on which it depends.
	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// Pallet ID.
		#[pallet::constant]
		type PalletId: Get<PalletId>;

		/// The overarching event type.
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// The currency trait.
		type Currency: Currency<Self::AccountId>;

		/// The balance type for assets (i.e. tokens).
		type AssetBalance: Balance
			+ FixedPointOperand
			+ MaxEncodedLen
			+ MaybeSerializeDeserialize
			+ TypeInfo;

		// Two-way conversion between asset and currency balances
		type AssetToCurrencyBalance: Convert<Self::AssetBalance, BalanceOf<Self>>;
		type CurrencyToAssetBalance: Convert<BalanceOf<Self>, Self::AssetBalance>;

		/// The asset ID type.
		type AssetId: MaybeSerializeDeserialize
			+ MaxEncodedLen
			+ TypeInfo
			+ Clone
			+ Debug
			+ PartialEq
			+ EncodeLike
			+ Decode;

		/// The type for tradable assets.
		type Assets: Inspect<Self::AccountId, AssetId = Self::AssetId, Balance = Self::AssetBalance>
			+ Transfer<Self::AccountId>;

		/// The type for liquidity tokens.
		type AssetRegistry: Inspect<Self::AccountId, AssetId = Self::AssetId, Balance = Self::AssetBalance>
			+ Mutate<Self::AccountId>
			+ Create<Self::AccountId>
			+ Destroy<Self::AccountId>;

		/// Information on runtime weights.
		// type WeightInfo: WeightInfo;

		/// Provider fee numerator.
		#[pallet::constant]
		type ProviderFeeNumerator: Get<BalanceOf<Self>>;

		/// Provider fee denominator.
		#[pallet::constant]
		type ProviderFeeDenominator: Get<BalanceOf<Self>>;

		/// Minimum currency deposit for a new exchange.
		#[pallet::constant]
		type MinDeposit: Get<BalanceOf<Self>>;
	}

	pub trait ConfigHelper: Config {
		fn pallet_account() -> AccountIdOf<Self>;
		fn currency_to_asset(curr_balance: BalanceOf<Self>) -> AssetBalanceOf<Self>;
		fn asset_to_currency(asset_balance: AssetBalanceOf<Self>) -> BalanceOf<Self>;
		fn net_amount_numerator() -> BalanceOf<Self>;
	}

	impl<T: Config> ConfigHelper for T {
		#[inline(always)]
		fn pallet_account() -> AccountIdOf<Self> {
			Self::PalletId::get().into_account_truncating()
		}

		#[inline(always)]
		fn currency_to_asset(curr_balance: BalanceOf<Self>) -> AssetBalanceOf<Self> {
			Self::CurrencyToAssetBalance::convert(curr_balance)
		}

		#[inline(always)]
		fn asset_to_currency(asset_balance: AssetBalanceOf<Self>) -> BalanceOf<Self> {
			Self::AssetToCurrencyBalance::convert(asset_balance)
		}

		#[inline(always)]
		fn net_amount_numerator() -> BalanceOf<Self> {
			Self::ProviderFeeDenominator::get()
				.checked_sub(&Self::ProviderFeeNumerator::get())
				.expect("Provider fee shouldn't be greater than 100%")
		}
	}

	type GenesisExchangeInfo<T> =
		(AccountIdOf<T>, AssetIdOf<T>, AssetIdOf<T>, BalanceOf<T>, AssetBalanceOf<T>);

	// Type alias for convenience
	type ExchangeOf<T> = Exchange<AssetIdOf<T>, BalanceOf<T>, AssetBalanceOf<T>>;

	// The pallet's runtime storage items.
	#[pallet::storage]
	#[pallet::getter(fn exchanges)]
	pub(super) type Exchanges<T: Config> =
		StorageMap<_, Twox64Concat, AssetIdOf<T>, ExchangeOf<T>, OptionQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A new exchange was created [asset_id, liquidity_token_id]
		ExchangeCreated(AssetIdOf<T>, AssetIdOf<T>),
		/// Liquidity was added to an exchange [provider_id, asset_id, currency_amount, token_amount, liquidity_minted]
		LiquidityAdded(
			T::AccountId,
			AssetIdOf<T>,
			BalanceOf<T>,
			AssetBalanceOf<T>,
			AssetBalanceOf<T>,
		),
		/// Liquidity was removed from an exchange [provider_id, asset_id, currency_amount, token_amount, liquidity_amount]
		LiquidityRemoved(
			T::AccountId,
			AssetIdOf<T>,
			BalanceOf<T>,
			AssetBalanceOf<T>,
			AssetBalanceOf<T>,
		),
		/// Currency was traded for an asset [asset_id, buyer_id, recipient_id, currency_amount, token_amount]
		CurrencyTradedForAsset(
			AssetIdOf<T>,
			T::AccountId,
			T::AccountId,
			BalanceOf<T>,
			AssetBalanceOf<T>,
		),
		/// An asset was traded for currency [asset_id, buyer_id, recipient_id, currency_amount, token_amount]
		AssetTradedForCurrency(
			AssetIdOf<T>,
			T::AccountId,
			T::AccountId,
			BalanceOf<T>,
			AssetBalanceOf<T>,
		),
	}

	#[pallet::error]
	pub enum Error<T> {
		/// Asset with the specified ID does not exist
		AssetNotFound,
		/// Exchange for the given asset already exists
		ExchangeAlreadyExists,
		/// Provided liquidity token ID is already taken
		TokenIdTaken,
		/// Not enough free balance to add liquidity or perform trade
		BalanceTooLow,
		/// Not enough tokens to add liquidity or perform trade
		NotEnoughTokens,
		/// Specified account doesn't own enough liquidity in the exchange
		ProviderLiquidityTooLow,
		/// No exchange found for the given `asset_id`
		ExchangeNotFound,
		/// Zero value provided for trade amount parameter
		TradeAmountIsZero,
		/// Zero value provided for `token_amount` parameter
		TokenAmountIsZero,
		/// Zero value provided for `max_tokens` parameter
		MaxTokensIsZero,
		/// Zero value provided for `currency_amount` parameter
		CurrencyAmountIsZero,
		/// Value provided for `currency_amount` parameter is too high
		CurrencyAmountTooHigh,
		/// Value provided for `currency_amount` parameter is too low
		CurrencyAmountTooLow,
		/// Zero value provided for `min_liquidity` parameter
		MinLiquidityIsZero,
		/// Value provided for `max_tokens` parameter is too low
		MaxTokensTooLow,
		/// Value provided for `min_liquidity` parameter is too high
		MinLiquidityTooHigh,
		/// Zero value provided for `liquidity_amount` parameter
		LiquidityAmountIsZero,
		/// Zero value provided for `min_currency` parameter
		MinCurrencyIsZero,
		/// Zero value provided for `min_tokens` parameter
		MinTokensIsZero,
		/// Value provided for `min_currency` parameter is too high
		MinCurrencyTooHigh,
		/// Value provided for `min_tokens` parameter is too high
		MinTokensTooHigh,
		/// Value provided for `max_currency` parameter is too low
		MaxCurrencyTooLow,
		/// Value provided for `min_bought_tokens` parameter is too high
		MinBoughtTokensTooHigh,
		/// Value provided for `max_sold_tokens` parameter is too low
		MaxSoldTokensTooLow,
		/// There is not enough liquidity in the exchange to perform trade
		NotEnoughLiquidity,
		/// Overflow occurred
		Overflow,
		/// Underflow occurred
		Underflow,
		/// Deadline specified for the operation has passed
		DeadlinePassed,
	}

	// Excgange Struct
	#[derive(
		Clone, Encode, Decode, Eq, PartialEq, RuntimeDebug, Default, MaxEncodedLen, TypeInfo,
	)]
	pub struct Exchange<AssetId, Balance, AssetBalance> {
		pub asset_id: AssetId,
		pub currency_reserve: Balance,
		pub token_reserve: AssetBalance,
		pub liquidity_token_id: AssetId,
	}
	// Dispatchable functions allows users to interact with the pallet and invoke state changes.
	// These functions materialize as "extrinsics", which are often compared to transactions.
	// Dispatchable functions must be annotated with a weight and must return a DispatchResult.
	#[pallet::call]
	impl<T: Config> Pallet<T> {
		#[pallet::weight(10_000 + T::DbWeight::get().writes(1).ref_time())]
		#[transactional]
		pub fn create_exchange(
			origin: OriginFor<T>,
			asset_id: AssetIdOf<T>,
			liquidity_token_id: AssetIdOf<T>,
			currency_amount: BalanceOf<T>,
			token_amount: AssetBalanceOf<T>,
		) -> DispatchResult {
			// -------------------------- Validation part --------------------------
			let caller = ensure_signed(origin)?;
			ensure!(currency_amount >= T::MinDeposit::get(), Error::<T>::CurrencyAmountTooLow);
			ensure!(token_amount > Zero::zero(), Error::<T>::TokenAmountIsZero);
			if T::Assets::total_issuance(asset_id.clone()).is_zero() {
				Err(Error::<T>::AssetNotFound)?
			}

			if <Exchanges<T>>::contains_key(asset_id.clone()) {
				Err(Error::<T>::ExchangeAlreadyExists)?
			}

			// ----------------------- Create liquidity token ----------------------
			T::AssetRegistry::create(
				liquidity_token_id.clone(),
				T::pallet_account(),
				false,
				<AssetBalanceOf<T>>::one(),
			)
			.map_err(|_| Error::<T>::TokenIdTaken)?;

			// -------------------------- Update storage ---------------------------
			let exchange = Exchange {
				asset_id: asset_id.clone(),
				currency_reserve: <BalanceOf<T>>::zero(),
				token_reserve: <AssetBalanceOf<T>>::zero(),
				liquidity_token_id: liquidity_token_id.clone(),
			};
			let liquidity_minted = T::currency_to_asset(currency_amount);

			Self::do_add_liquidity(
				exchange,
				currency_amount,
				token_amount,
				liquidity_minted,
				caller,
			)?;

			Self::deposit_event(Event::ExchangeCreated(asset_id, liquidity_token_id));
			Ok(())
		}
	}

	impl<T: Config> Pallet<T> {
		/// Perform currency and asset transfers, mint liquidity token,
		/// update exchange balances, emit event
		#[transactional]
		pub fn do_add_liquidity(
			mut exchange: ExchangeOf<T>,
			currency_amount: BalanceOf<T>,
			token_amount: AssetBalanceOf<T>,
			liquidity_minted: AssetBalanceOf<T>,
			provider: AccountIdOf<T>,
		) -> DispatchResult {
			let asset_id = exchange.asset_id.clone();
			let pallet_account = T::pallet_account();
			<T as pallet::Config>::Currency::transfer(
				&provider,
				&pallet_account,
				currency_amount,
				ExistenceRequirement::KeepAlive,
			)?;

			T::Assets::transfer(asset_id.clone(), &provider, &pallet_account, token_amount, true)?;
			T::AssetRegistry::mint_into(
				exchange.liquidity_token_id.clone(),
				&provider,
				liquidity_minted,
			)?;

			// -------------------------- Balances update --------------------------

			exchange.currency_reserve.saturating_accrue(currency_amount);
			exchange.token_reserve.saturating_accrue(token_amount);
			<Exchanges<T>>::insert(asset_id.clone(), exchange);

			// ---------------------------- Emit event -----------------------------

			Self::deposit_event(Event::LiquidityAdded(
				provider,
				asset_id,
				currency_amount,
				token_amount,
				liquidity_minted,
			));
			Ok(())
		}
	}
}
