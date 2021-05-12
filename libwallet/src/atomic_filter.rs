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

use crate::grin_core::ser::{self as grin_ser, Readable, Reader, Writeable, Writer};
use scalable_cuckoo_filter::ScalableCuckooFilter;
use serde_json;

/// Atomic nonce filter to ensure no reuse of atomic nonces
pub struct AtomicFilter(ScalableCuckooFilter<u64>);

impl AtomicFilter {
	/// Create a new atomic nonce filter with a given initial length and false-positive rate
	pub fn new(filter_len: usize, filter_rate: f64) -> Self {
		Self(ScalableCuckooFilter::new(filter_len, filter_rate))
	}

	/// Insert an entry in the filter
	pub fn insert(&mut self, item: u64) {
		self.0.insert(&item);
	}

	/// Get whether the filter contains an entry
	pub fn contains(&self, item: u64) -> bool {
		self.0.contains(&item)
	}
}

impl Writeable for AtomicFilter {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), grin_ser::Error> {
		writer.write_u32(self.0.len() as u32)?;
		writer.write_fixed_bytes(
			&serde_json::to_vec(&self.0).map_err(|e| std::io::Error::from(e))?,
		)?;
		Ok(())
	}
}

impl Readable for AtomicFilter {
	fn read<R: Reader>(reader: &mut R) -> Result<Self, grin_ser::Error> {
		let filter_len = reader.read_u32()?;
		let filter_bytes = reader.read_fixed_bytes(filter_len as usize)?;
		Ok(Self(
			serde_json::from_slice(&filter_bytes).map_err(|e| std::io::Error::from(e))?,
		))
	}
}
