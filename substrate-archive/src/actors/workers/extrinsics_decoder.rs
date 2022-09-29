// Copyright 2017-2021 Parity Technologies (UK) Ltd.
// This file is part of substrate-archive.

// substrate-archive is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
// substrate-archive is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with substrate-archive.  If not, see <http://www.gnu.org/licenses/>.

use arc_swap::ArcSwap;
use async_std::task;
use itertools::Itertools;
use sqlx::PgPool;
use std::{collections::HashMap, sync::Arc};
use xtra::prelude::*;

use desub::Decoder;
use log::info;
use serde_json::Value;

use crate::{
	actors::{
		workers::database::{DatabaseActor, GetState},
		SystemConfig,
	},
	database::{models::BucketModel, models::ExtrinsicsModel, queries},
	error::{ArchiveError, Result},
	types::{BatchBuckets, BatchExtrinsics},
};

use crate::database::BlockDigestModel;
use codec::{Decode, Encode};
use sa_work_queue::{BackgroundJob, Runner};
use serde::{Deserialize, Serialize};

pub static TASK_QUEUE_EXT: &str = "SA_QUEUE_EXT";
pub static EXT_JOB_TYPE: &str = "Strategy_job";
pub static DIGEST_JOB_TYPE: &str = "Digest_job";

/// Actor which crawls missing encoded extrinsics and
/// sends decoded JSON to the database.
/// Crawls missing extrinsics upon receiving an `Index` message.
pub struct ExtrinsicsDecoder {
	/// Pool of Postgres Connections.
	pool: PgPool,
	/// Address of the database actor.
	addr: Address<DatabaseActor>,
	/// Max amount of extrinsics to load at any one time.i.
	max_block_load: u32,
	/// Desub Legacy + current decoder.
	decoder: Arc<Decoder>,
	/// Cache of blocks where runtime upgrades occurred.
	/// number -> spec
	upgrades: ArcSwap<HashMap<u32, u32>>,
	/// URL for RabbitMQ. Default is localhost:5672
	task_url: String,
}

#[derive(Deserialize, Debug)]
struct AccountId(Vec<Vec<u8>>);

#[derive(Deserialize, Debug)]
struct CipherText(Vec<u8>);

#[derive(Deserialize, Debug)]
struct Ciphers(Vec<Cipher>);

#[derive(Debug, Deserialize, Encode, Decode)]
struct Cipher {
	cipher_text: String,
	difficulty: u32,
	release_block_num: u32,
}

impl ExtrinsicsDecoder {
	pub async fn new<B: Send + Sync, Db: Send + Sync>(
		config: &SystemConfig<B, Db>,
		addr: Address<DatabaseActor>,
	) -> Result<Self> {
		let max_block_load = config.clone().control.max_block_load;
		let pool = addr.send(GetState::Pool).await??.pool();
		let task_url = config.clone().control.task_url;
		// let chain = config.persistent_config.chain();
		// let decoder = Arc::new(Decoder::new(chain));
		let decoder = Arc::new(Decoder::new());
		let mut conn = pool.acquire().await?;
		let upgrades = ArcSwap::from_pointee(queries::upgrade_blocks_from_spec(&mut conn, 0).await?);
		log::info!("Started extrinsic decoder");
		Ok(Self { pool, addr, max_block_load, decoder, upgrades, task_url })
	}

	async fn crawl_missing_extrinsics(&mut self) -> Result<()> {
		let mut conn = self.pool.acquire().await?;
		let blocks = queries::blocks_missing_extrinsics(&mut conn, self.max_block_load).await?;

		let versions: Vec<u32> =
			blocks.iter().filter(|b| !self.decoder.has_version(&b.3)).map(|(_, _, _, v, _)| *v).unique().collect();
		// above and below line are separate to let immutable ref to `self.decoder` to go out of scope.
		for version in versions.iter() {
			let metadata = queries::metadata(&mut conn, *version as i32).await?;
			log::debug!("Registering version {}", version);
			Arc::get_mut(&mut self.decoder)
				.ok_or_else(|| ArchiveError::Msg("Reference to decoder is not safe to access".into()))?
				.register_version(*version, &metadata)?;
		}

		if let Some(first) = versions.first() {
			if let (Some(past), _, Some(past_metadata), _) =
				queries::past_and_present_version(&mut conn, *first as i32).await?
			{
				Arc::get_mut(&mut self.decoder)
					.ok_or_else(|| ArchiveError::Msg("Reference to decoder is not safe to access".into()))?
					.register_version(past, &past_metadata)?;
				log::debug!("Registered previous version {}", past);
			}
		}

		if self.upgrades.load().iter().max_by(|a, b| a.1.cmp(b.1)).map(|(_, v)| v)
			< blocks.iter().map(|&(_, _, _, v, _)| v).max().as_ref()
		{
			self.update_upgrade_blocks().await?;
		}
		let decoder = self.decoder.clone();
		let upgrades = self.upgrades.load().clone();

		let extrinsics_tuple =
			task::spawn_blocking(move || Ok::<_, ArchiveError>(Self::decode(&decoder, blocks.clone(), &upgrades)))
				.await??;

		let extrinsics = extrinsics_tuple.0;
		self.addr.send(BatchExtrinsics::new(extrinsics)).await?;

		//send batch buckets to DatabaseActor
		let buckets = extrinsics_tuple.1;
		if &buckets.len() > &0 {
			dbg!("I have one or more buckets");
			self.publish_generic::<BucketModel>(&buckets, EXT_JOB_TYPE);
		}
		self.addr.send(BatchBuckets::new(buckets)).await?;

		//send block headers to Rabbitmq
		let block_headers = extrinsics_tuple.2;
		if &block_headers.len() > &0 {
			dbg!("I have one or more blocks");
			self.publish_generic::<BlockDigestModel>(&block_headers, DIGEST_JOB_TYPE);
		}

		Ok(())
	}

	fn publish_generic<B: Serialize>(&self, list: &Vec<B>, job_type: &str) {
		let runner = Self::runner(self);
		for item in list {
			Self::create_generic_job::<B>(&runner, item, job_type);
		}
	}

	fn create_generic_job<B: Serialize>(runner: &Runner<()>, item: &B, job_type: &str)
	where
		B: Serialize,
	{
		let data = serde_json::to_string(item).unwrap_or("".to_string());
		let job = BackgroundJob { job_type: job_type.into(), data: serde_json::from_str(&*data).unwrap() };
		let handle = runner.handle();
		task::block_on(handle.push(serde_json::to_vec(&job).unwrap_or(Default::default())));
	}

	fn runner(&self) -> Runner<()> {
		Runner::builder((), &self.task_url)
			.num_threads(2)
			.timeout(std::time::Duration::from_secs(5))
			.queue_name(TASK_QUEUE_EXT)
			.prefetch(1) // high prefetch values will screw tests up
			.build()
			.unwrap()
	}

	fn decode(
		decoder: &Decoder,
		blocks: Vec<(u32, Vec<u8>, Vec<u8>, u32, Vec<u8>)>,
		upgrades: &HashMap<u32, u32>,
	) -> Result<(Vec<ExtrinsicsModel>, Vec<BucketModel>, Vec<BlockDigestModel>)> {
		let mut extrinsics = Vec::new();
		let mut buckets = Vec::new();
		let mut block_headers = Vec::new();
		if blocks.len() > 2 {
			let first = blocks.first().expect("Checked len; qed");
			let last = blocks.last().expect("Checked len; qed");
			log::info!(
				"Decoding {} extrinsics in blocks {}..{} versions {}..{}",
				blocks.len(),
				first.0,
				last.0,
				first.3,
				last.3
			);
		}
		for (number, hash, ext, spec, digest) in blocks.into_iter() {
			if let Some(version) = upgrades.get(&number) {
				let previous = upgrades
					.values()
					.sorted()
					.tuple_windows()
					.find(|(_curr, next)| *next >= version)
					.map(|(c, _)| c)
					.ok_or(ArchiveError::PrevSpecNotFound(*version))?;

				match decoder.decode_extrinsics(*previous, ext.as_slice()) {
					Ok(exts) => {
						//construct bucket list for batch
						Self::construct_buckets(&number, &hash, &exts, &mut buckets);
						Self::construct_block_digest(&number, &exts, &digest, &mut block_headers);
						if let Ok(exts_model) = ExtrinsicsModel::new(hash, number, exts) {
							extrinsics.push(exts_model);
						}
					}
					Err(err) => {
						log::warn!(
							"decode extrinsic upgrade failed, block: {}, spec: {}, reason: {:?}",
							number,
							spec,
							err
						);
					}
				}
			} else {
				match decoder.decode_extrinsics(spec, ext.as_slice()) {
					Ok(exts) => {
						//construct bucket list for batch
						Self::construct_buckets(&number, &hash, &exts, &mut buckets);
						Self::construct_block_digest(&number, &exts, &digest, &mut block_headers);
						if let Ok(exts_model) = ExtrinsicsModel::new(hash, number, exts) {
							extrinsics.push(exts_model);
						}
					}
					Err(err) => {
						log::warn!("decode extrinsic failed, block: {}, spec: {}, reason: {:?}", number, spec, err);
					}
				}
			}
		}
		Ok((extrinsics, buckets, block_headers))
	}

	//construct brief block list for batch
	fn construct_block_digest(number: &u32, ext: &Value, digest: &Vec<u8>, block_headers: &mut Vec<BlockDigestModel>) {
		if ext.is_array() {
			let extrinsics = ext.as_array().unwrap();
			for extrinsic in extrinsics {
				if extrinsic.is_object() {
					//This operation will always succeed,because the type has been determined.
					let extrinsic_map = extrinsic.as_object().unwrap();
					//Do not exclude empty string.
					let pallet_name = extrinsic_map["call_data"]["pallet_name"].as_str().unwrap_or("");
					let arguments = extrinsic_map["call_data"]["arguments"].to_owned();
					match pallet_name {
						"Timestamp" => {
							let timestamp: u128 = serde_json::from_value(arguments[0].to_owned()).unwrap_or(0u128);
							let block_model =
								BlockDigestModel { block_num: number.to_owned(), digest: digest.to_owned(), timestamp };
							block_headers.push(block_model);
						}
						"TREXModule" => {}
						_ => {
							log::debug! {"Other type of Extrinsic!"};
						}
					}
				}
			}
		}
	}

	//construct bucket list for batch
	fn construct_buckets(number: &u32, hash: &Vec<u8>, ext: &Value, buckets: &mut Vec<BucketModel>) {
		if ext.is_array() {
			let extrinsics = ext.as_array().unwrap();
			for extrinsic in extrinsics {
				if extrinsic.is_object() {
					//This operation will always succeed,because the type has been determined.
					let extrinsic_map = extrinsic.as_object().unwrap();
					//Do not exclude empty string.
					let pallet_name = extrinsic_map["call_data"]["pallet_name"].as_str().unwrap_or("");
					let arguments = extrinsic_map["call_data"]["arguments"].to_owned();
					match pallet_name {
						"Timestamp" => {
							// TODO:Extrinsic of timestamp type to be determined,If is needed.
						}
						"TREXModule" => {
							// get account_id from arguments[0]
							let account_id_struct: Option<AccountId> =
								serde_json::from_value(arguments[0].to_owned()).unwrap_or(None);
							let account_id = match account_id_struct {
								Some(value) => Some(value.0),
								None => None,
							};

							// get cipher_text from arguments[1]
							let option_cipher_text_struct: Option<CipherText> =
								serde_json::from_value(arguments[1].to_owned()).unwrap_or(None);
							// get json string from matching chiper_text
							if let Some(cipher_encoded) = option_cipher_text_struct {
								let cipher_decoded = Cipher::decode(&mut &cipher_encoded.0[..]);
								if let Ok(cipher) = cipher_decoded {
									println!("!!!!!!!!!!!!!!!!!!!{:?}", cipher);
									let cipher_text = cipher.cipher_text.as_bytes();
									let release_block_number = cipher.release_block_num;
									let difficulty = cipher.difficulty;
									let release_block_difficulty_index =
										cipher.release_block_num.to_string()
											+ &"_".to_string() + &cipher.difficulty.to_string();

									let bucket_model_result = BucketModel::new(
										hash.to_vec(),
										number.to_owned(),
										Option::from(cipher_text.to_vec()),
										account_id.to_owned(),
										&pallet_name,
										Option::from(release_block_number),
										difficulty,
										release_block_difficulty_index,
									);
									match bucket_model_result {
										Ok(bucket_model) => {
											buckets.push(bucket_model);
										}
										Err(_) => {
											log::debug! {"Construct bucket model failed!"};
										}
									}
								}
							}
						}
						_ => {
							log::debug! {"Other type of Extrinsic!"};
						}
					}
				}
			}
		}
	}

	async fn update_upgrade_blocks(&self) -> Result<()> {
		let max_spec = *self.upgrades.load().iter().max_by(|a, b| a.1.cmp(b.1)).map(|(k, _)| k).unwrap_or(&0);
		let mut conn = self.pool.acquire().await?;
		let upgrade_blocks = queries::upgrade_blocks_from_spec(&mut conn, max_spec).await?;
		self.upgrades.rcu(move |upgrades| {
			let mut upgrades = HashMap::clone(upgrades);
			upgrades.extend(upgrade_blocks.iter());
			upgrades
		});
		Ok(())
	}
}

#[async_trait::async_trait]
impl Actor for ExtrinsicsDecoder {}

pub struct Index;
impl Message for Index {
	type Result = ();
}

#[async_trait::async_trait]
impl Handler<Index> for ExtrinsicsDecoder {
	async fn handle(&mut self, _: Index, ctx: &mut Context<Self>) {
		match self.crawl_missing_extrinsics().await {
			Err(ArchiveError::Disconnected) => ctx.stop(),
			Ok(_) => {}
			Err(e) => log::error!("{:?}", e),
		}
	}
}
