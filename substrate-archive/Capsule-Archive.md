# trex-archive

#### 1.First, solve the problem of creating the transaction table in the PostgreSQL database.
Table Name:trexes

Table design: 
```sql
CREATE TABLE IF NOT EXISTS extrinsics (
	id SERIAL NOT NULL,
	hash bytea NOT NULL PRIMARY KEY,
	number int check (number >= 0 and number < 2147483647) NOT NULL UNIQUE,
	extrinsics jsonb NOT NULL
);
```

> Refer to substrate-archive/src/database/models.rs as below:
```rust
#[derive(Debug, Serialize, FromRow)]
pub struct BucketModel{
	pub id: Option<i32>,
	pub hash: Vec<u8>,
	pub number: u32,
	pub cipher:Option<Vec<u8>>,
	pub account_id:Option<Vec<Vec<u8>>>,
	pub app_prefix:String,
	pub release_number: Option<u32>
}


impl BucketModel {
	pub fn new(block_id: Vec<u8>, block_num: u32, cipher: Option<Vec<u8>>, account_id:Option<Vec<Vec<u8>>>, app_prefix:Vec<u8>, release_number:Option<u32>) -> Result<Self>{
		let block_id = block_id.try_into().unwrap_or(vec![]);
		let block_num = block_num.try_into().unwrap_or(0u32);
		let app_prefix = String::from_utf8(app_prefix).unwrap_or(String::from(""));
		Ok(Self{id: None, hash:block_id, number:block_num,cipher,account_id,app_prefix,release_number})
	}
}
```

#### 2.Resolve type addition in defined type.
> Refer to substrate-archive/src/types.rs as below:

```rust
#[derive(Debug)]
pub struct  BatchBuckets {
	pub inner: Vec<TrexModel>,
}

impl BatchBuckets {
	pub fn new(buckets: Vec<BucketModel>) -> Self { Self { inner: buckets}}

	pub fn inner(self) -> Vec<BucketModel> { self.inner}

	pub fn len(&self) -> usize { self.inner.len() }
}

impl Message for BatchBuckets {
	type Result = ();
}
```

#### 3.Because the fetching of the data source of externalics is homologous with the data of buckets, in order to avoid repeated fetching, the scheme is to organize Vec<BucketModel> in the actor of ExtrinsicsDecoder.
> Refer to substrate-archive/src/actors/workers/extrinsics_decoder.rs -> fn crawl_missing_extrinsics as below:
```rust
let extrinsics_tuple = task::spawn_blocking(move || Ok::<_, ArchiveError>(Self::decode(&decoder, blocks, &upgrades))).await??;

let extrinsics= extrinsics_tuple.0;
self.addr.send(BatchExtrinsics::new(extrinsics)).await?;

//send batch buckets to DatabaseActor
let buckets = extrinsics_tuple.1;
self.addr.send(BatchBuckets::new(buckets)).await?;
```
> This operation will send extrinsics and buckets to DatabaseActor.

#### 4.The organized Vec< BucketModel > needs to be sent to DatabaseActor for processing.
> Refer to substrate-archive/src/actors/workers/database.rs as below:
```rust
#[async_trait::async_trait]
impl Handler<BatchBuckets> for DatabaseActor {
	async fn handle(&mut self, buckets: BatchBuckets, _: &mut Context<Self>) {
		let len = buckets.len();
		let now = std::time::Instant::now();
		if let Err(e) = self.db.insert(buckets.inner()).await {
			log::error!("{}", e.to_string());
		}
		log::debug!("took {:?} to insert {} buckets", now.elapsed(), len);
	}
}
```

#### 5.Implement the insert trait for BucketModel struct.
> Refer to substrate-archive/src/database.rs as below:
```rust
#[async_trait::async_trait]
impl Insert for Vec<BucketModel> {
	async fn insert(mut self, conn: &mut DbConn) -> DbReturn {
		let mut batch = Batch::new(
			"bucket",
			r#"
			INSERT INTO "bucket" (
				hash, number, cipher, account_id, app_prefix, release_number
			) VALUES
			"#,
			r#"
			ON CONFLICT DO NOTHING
			"#,
		);
		for bucket in self.into_iter() {
			batch.reserve(6)?;
			if batch.current_num_arguments() > 0 {
				batch.append(",");
			}
			batch.append("(");
			batch.bind(bucket.hash)?;
			batch.append(",");
			batch.bind(bucket.number)?;
			batch.append(",");
			batch.bind(bucket.cipher)?;
			batch.append(",");
			batch.bind(bucket.account_id)?;
			batch.append(",");
			batch.bind(bucket.app_prefix)?;
			batch.append(",");
			batch.bind(bucket.release_number)?;
			batch.append(")");
		}
		Ok(batch.execute(conn).await?)
	}
}
```
