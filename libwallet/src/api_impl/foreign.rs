// Copyright 2021 The Grin Developers
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

//! Generic implementation of owner API functions
use strum::IntoEnumIterator;

use crate::api_impl::owner::{check_ttl, post_tx};
use crate::api_impl::owner::{finalize_atomic_swap, finalize_tx as owner_finalize};
use crate::grin_core::core::FeeFields;
use crate::grin_keychain::{Keychain, SwitchCommitmentType};
use crate::grin_util::secp::key::SecretKey;
use crate::internal::{selection, tx, updater};
use crate::slate_versions::SlateVersion;
use crate::{
	address, AtomicFilter, BlockFees, CbData, Error, ErrorKind, NodeClient, Slate, SlateState,
	TxLogEntryType, VersionInfo, WalletBackend,
};

const FOREIGN_API_VERSION: u16 = 2;

/// Return the version info
pub fn check_version() -> VersionInfo {
	VersionInfo {
		foreign_api_version: FOREIGN_API_VERSION,
		supported_slate_versions: SlateVersion::iter().collect(),
	}
}

/// Build a coinbase transaction
pub fn build_coinbase<'a, T: ?Sized, C, K>(
	w: &mut T,
	keychain_mask: Option<&SecretKey>,
	block_fees: &BlockFees,
	test_mode: bool,
) -> Result<CbData, Error>
where
	T: WalletBackend<'a, C, K>,
	C: NodeClient + 'a,
	K: Keychain + 'a,
{
	updater::build_coinbase(&mut *w, keychain_mask, block_fees, test_mode)
}

/// Receive a tx as recipient
pub fn receive_tx<'a, T: ?Sized, C, K>(
	w: &mut T,
	keychain_mask: Option<&SecretKey>,
	slate: &Slate,
	dest_acct_name: Option<&str>,
	use_test_rng: bool,
) -> Result<Slate, Error>
where
	T: WalletBackend<'a, C, K>,
	C: NodeClient + 'a,
	K: Keychain + 'a,
{
	let mut ret_slate = slate.clone();
	check_ttl(w, &ret_slate)?;
	let parent_key_id = match dest_acct_name {
		Some(d) => {
			let pm = w.get_acct_path(d.to_owned())?;
			match pm {
				Some(p) => p.path,
				None => w.parent_key_id(),
			}
		}
		None => w.parent_key_id(),
	};
	// Don't do this multiple times
	let tx = updater::retrieve_txs(
		&mut *w,
		None,
		Some(ret_slate.id),
		Some(&parent_key_id),
		use_test_rng,
	)?;
	for t in &tx {
		if t.tx_type == TxLogEntryType::TxReceived {
			return Err(ErrorKind::TransactionAlreadyReceived(ret_slate.id.to_string()).into());
		}
	}

	ret_slate.tx = Some(Slate::empty_transaction());

	let height = w.last_confirmed_height()?;
	let keychain = w.keychain(keychain_mask)?;

	let context = tx::add_output_to_slate(
		&mut *w,
		keychain_mask,
		&mut ret_slate,
		height,
		&parent_key_id,
		false,
		use_test_rng,
	)?;

	// Add our contribution to the offset
	ret_slate.adjust_offset(&keychain, &context)?;

	let excess = ret_slate.calc_excess(keychain.secp())?;

	if let Some(ref mut p) = ret_slate.payment_proof {
		let sig = tx::create_payment_proof_signature(
			ret_slate.amount,
			&excess,
			p.sender_address,
			address::address_from_derivation_path(&keychain, &parent_key_id, 0)?,
		)?;

		p.receiver_signature = Some(sig);
	}

	ret_slate.amount = 0;
	ret_slate.fee_fields = FeeFields::zero();
	ret_slate.remove_other_sigdata(&keychain, &context.sec_nonce, &context.sec_key)?;
	ret_slate.state = SlateState::Standard2;

	Ok(ret_slate)
}

/// Receive an atomic tx as recipient
pub fn receive_atomic_tx<'a, T: ?Sized, C, K>(
	w: &mut T,
	keychain_mask: Option<&SecretKey>,
	slate: &Slate,
	dest_acct_name: Option<&str>,
	use_test_rng: bool,
) -> Result<Slate, Error>
where
	T: WalletBackend<'a, C, K>,
	C: NodeClient + 'a,
	K: Keychain + 'a,
{
	let mut ret_slate = slate.clone();
	check_ttl(w, &ret_slate)?;
	let parent_key_id = match dest_acct_name {
		Some(d) => {
			let pm = w.get_acct_path(d.to_owned())?;
			match pm {
				Some(p) => p.path,
				None => w.parent_key_id(),
			}
		}
		None => w.parent_key_id(),
	};
	// Don't do this multiple times
	let tx = updater::retrieve_txs(
		w,
		None,
		Some(ret_slate.id),
		Some(&parent_key_id),
		use_test_rng,
	)?;
	for t in &tx {
		if t.tx_type == TxLogEntryType::TxReceived {
			return Err(ErrorKind::TransactionAlreadyReceived(ret_slate.id.to_string()).into());
		}
	}

	ret_slate.tx = Some(Slate::empty_transaction());

	let height = w.last_confirmed_height()?;
	let keychain = w.keychain(keychain_mask)?;

	let is_height_lock = ret_slate.kernel_features == 2;
	// derive atomic nonce from the slate's `atomic_id`
	let atomic_nonce = {
		let atomic_id = match &ret_slate.atomic_id {
			Some(aid) => aid.clone(),
			None => return Err(ErrorKind::GenericError("missing atomic ID".into()).into()),
		};
		let atomic_int = Slate::atomic_id_to_int(&atomic_id)?;
		let mut filter = match w.get_atomic_filter(keychain_mask) {
			Ok(f) => f,
			Err(_) => AtomicFilter::new(100, 0.001),
		};
		if filter.contains(atomic_int) {
			return Err(ErrorKind::GenericError("atomic nonce already used".into()).into());
		}
		filter.insert(atomic_int);
		let atomic =
			keychain.derive_key(ret_slate.amount, &atomic_id, SwitchCommitmentType::Regular)?;

		let mut batch = w.batch(keychain_mask)?;
		batch.save_atomic_nonce(&atomic_id, &atomic)?;
		batch.save_atomic_filter(&filter)?;
		batch.commit()?;

		Some(atomic)
	};

	let (input_ids, output_ids) = if is_height_lock {
		// add input(s) and change output to slate
		let ctx = tx::add_inputs_to_atomic_slate(
			w,
			keychain_mask,
			&mut ret_slate,
			height,
			10,   // min_confirmations
			500,  // max_outputs
			1,    // num_change_outputs
			true, // selection_strategy_is_use_all
			&parent_key_id,
			atomic_nonce.clone(),
			use_test_rng,
		)?;

		(ctx.input_ids, ctx.output_ids)
	} else {
		(vec![], vec![])
	};

	let mut context = tx::add_output_to_atomic_slate(
		w,
		keychain_mask,
		&mut ret_slate,
		height,
		&parent_key_id,
		atomic_nonce,
		use_test_rng,
	)?;

	context.fee = Some(ret_slate.fee_fields.clone());
	let excess = ret_slate.calc_excess(keychain.secp())?;

	if let Some(ref mut p) = ret_slate.payment_proof {
		let sig = tx::create_payment_proof_signature(
			ret_slate.amount,
			&excess,
			p.sender_address,
			address::address_from_derivation_path(&keychain, &parent_key_id, 0)?,
		)?;

		p.receiver_signature = Some(sig);
	}

	if is_height_lock {
		ret_slate.compact()?;
		context.input_ids = input_ids;
		context.output_ids.extend_from_slice(&output_ids);
		let mut batch = w.batch(keychain_mask)?;
		batch.save_private_context(ret_slate.id.as_bytes(), &context)?;
		batch.commit()?;
	}

	ret_slate.adjust_offset(&keychain, &context)?;
	ret_slate.state = SlateState::Atomic2;

	Ok(ret_slate)
}

/// Receive a tx that this wallet has issued
pub fn finalize_tx<'a, T: ?Sized, C, K>(
	w: &mut T,
	keychain_mask: Option<&SecretKey>,
	slate: &Slate,
	post_automatically: bool,
) -> Result<Slate, Error>
where
	T: WalletBackend<'a, C, K>,
	C: NodeClient + 'a,
	K: Keychain + 'a,
{
	let mut sl = slate.clone();
	let context = w.get_private_context(keychain_mask, sl.id.as_bytes())?;
	if sl.state == SlateState::Invoice2 {
		check_ttl(w, &sl)?;

		// Add our contribution to the offset
		sl.adjust_offset(&w.keychain(keychain_mask)?, &context)?;

		let mut temp_ctx = context.clone();
		temp_ctx.sec_key = context.initial_sec_key.clone();
		temp_ctx.sec_nonce = context.initial_sec_nonce.clone();
		selection::repopulate_tx(&mut *w, keychain_mask, &mut sl, &temp_ctx, false)?;

		tx::complete_tx(&mut *w, keychain_mask, &mut sl, &context)?;
		tx::update_stored_tx(&mut *w, keychain_mask, &context, &mut sl, true)?;
		{
			let mut batch = w.batch(keychain_mask)?;
			batch.delete_private_context(sl.id.as_bytes())?;
			batch.commit()?;
		}
		sl.state = SlateState::Invoice3;
		sl.amount = 0;
	} else if sl.state == SlateState::Atomic3 {
		sl = finalize_atomic_swap(w, keychain_mask, slate)?;
	} else {
		sl = owner_finalize(w, keychain_mask, slate)?;
	}
	if post_automatically {
		post_tx(w.w2n_client(), sl.tx_or_err()?, true)?;
	}
	Ok(sl)
}
