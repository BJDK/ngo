use block_device::{BioReq, BioReqBuilder, BioResp, BioSubmission, BioType, BlockBuf, BlockDevice};
#[cfg(feature = "sgx")]
use sgx_tcrypto::{rsgx_rijndael128GCM_decrypt, rsgx_rijndael128GCM_encrypt};
#[cfg(feature = "sgx")]
use sgx_types::sgx_status_t;

use crate::prelude::*;

/// An encrypted disk.
///
/// A decorator type that adds a layer of encryption atop any other disk.
/// This implementation is insecure; it is only intended for performance
/// profiling.
pub struct CryptDisk {
    inner: Box<dyn BlockDevice>,
}

impl CryptDisk {
    pub fn new(inner: Box<dyn BlockDevice>) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> &dyn BlockDevice {
        &*self.inner
    }

    fn do_read(&self, origin_req: Arc<BioReq>) {
        fn on_complete(new_req: &BioReq, resp: &BioResp) {
            let origin_req = new_req.ext().remove::<OriginReq>().unwrap().into_inner();

            if resp.is_ok() {
                // Decrypt the data
                new_req.access_bufs_with(|bufs| {
                    let merged_buf = bufs[0].as_slice();
                    origin_req.access_mut_bufs_with(|bufs| {
                        let mut copied_len = 0;
                        for buf in bufs {
                            dummy_decrypt(
                                &merged_buf[copied_len..copied_len + buf.len()],
                                buf.as_slice_mut(),
                            );
                            copied_len += buf.len();
                        }
                    });
                });
            }

            // Notify the origin request I/O completed
            unsafe {
                origin_req.complete(*resp);
            }
        }

        let new_req = Self::new_req_builder(&origin_req)
            .on_complete(on_complete)
            .ext(OriginReq::new(origin_req.clone()))
            .build();

        let _ = self.inner.submit(Arc::new(new_req));
    }

    fn do_write(&self, origin_req: Arc<BioReq>) {
        fn on_complete(new_req: &BioReq, resp: &BioResp) {
            // Notify the origin request I/O completed
            let origin_req = new_req.ext().remove::<OriginReq>().unwrap().into_inner();
            unsafe {
                origin_req.complete(*resp);
            }
        }

        let new_req = Self::new_req_builder(&origin_req)
            .on_complete(on_complete)
            .ext(OriginReq::new(origin_req.clone()))
            .build();

        // Encrypt the data
        new_req.access_mut_bufs_with(|bufs| {
            let merged_buf = bufs[0].as_slice_mut();
            origin_req.access_bufs_with(|bufs| {
                let mut copied_len = 0;
                for buf in bufs {
                    dummy_encrypt(
                        buf.as_slice(),
                        &mut merged_buf[copied_len..copied_len + buf.len()],
                    );
                    copied_len += buf.len();
                }
            });
        });

        let _ = self.inner.submit(Arc::new(new_req));
    }

    fn do_flush(&self, req: Arc<BioReq>) -> BioSubmission {
        self.inner.submit(req)
    }

    fn new_req_builder(origin_req: &Arc<BioReq>) -> BioReqBuilder {
        fn new_merged_buf(origin_req: &Arc<BioReq>) -> BlockBuf {
            origin_req.access_bufs_with(|bufs| {
                let total_len = bufs.iter().map(|buf| buf.len()).sum();
                let uninit_slice = Box::new_uninit_slice(total_len);
                // Safety. The initiail content is not important now.
                let boxed_slice = unsafe { uninit_slice.assume_init() };
                BlockBuf::from_boxed(boxed_slice)
            })
        }

        fn drop_merged_buf(_new_req: &BioReq, mut bufs: Vec<BlockBuf>) {
            debug_assert!(bufs.len() == 1);
            let block_buf = bufs.remove(0);
            drop(unsafe { BlockBuf::into_boxed(block_buf) });
        }

        BioReqBuilder::new(origin_req.type_())
            .addr(origin_req.addr())
            .bufs({
                let merged_buf = new_merged_buf(origin_req);
                vec![merged_buf]
            })
            .on_drop(drop_merged_buf)
    }
}

impl BlockDevice for CryptDisk {
    fn total_blocks(&self) -> usize {
        self.inner.total_blocks()
    }

    fn submit(&self, req: Arc<BioReq>) -> BioSubmission {
        // For reads and writes, we will create a new request and submit it to
        // intern disk. For flushes, we just redirect the request to the intern
        // disk, without creating a submission object (we cannot create multiple
        // submissions out of one request).
        let type_ = req.type_();
        if type_ != BioType::Flush {
            // Update the status of req to submittted
            let submission = BioSubmission::new(req);

            let req = submission.req().clone();
            match type_ {
                BioType::Read => self.do_read(req),
                BioType::Write => self.do_write(req),
                _ => unreachable!(),
            };

            submission
        } else {
            self.do_flush(req)
        }
    }
}

/// A new-type wrapper to be used in AnyMap.
#[repr(transparent)]
#[derive(Debug)]
struct OriginReq(Arc<BioReq>);

impl OriginReq {
    pub fn new(req: Arc<BioReq>) -> Self {
        Self(req)
    }

    pub fn into_inner(self) -> Arc<BioReq> {
        self.0
    }
}

#[cfg(feature = "sgx")]
fn dummy_encrypt(plaintext: &[u8], ciphertext: &mut [u8]) {
    let mut gmac = [0; 16];
    let aes_key: [u8; 16] = [1; 16];
    let nonce: [u8; 12] = [2; 12];
    let aad: [u8; 0] = [0; 0];
    rsgx_rijndael128GCM_encrypt(&aes_key, plaintext, &nonce, &aad, ciphertext, &mut gmac).unwrap();
}

#[cfg(not(feature = "sgx"))]
fn dummy_encrypt(plaintext: &[u8], ciphertext: &mut [u8]) {
    // TODO: do encryption
    ciphertext.copy_from_slice(plaintext);
}

#[cfg(feature = "sgx")]
fn dummy_decrypt(ciphertext: &[u8], plaintext: &mut [u8]) {
    let gmac = [0; 16];
    let aes_key: [u8; 16] = [1; 16];
    let nonce: [u8; 12] = [2; 12];
    let aad: [u8; 0] = [0; 0];
    let sgx_res = rsgx_rijndael128GCM_decrypt(&aes_key, ciphertext, &nonce, &aad, &gmac, plaintext);
    match sgx_res {
        Ok(()) => (),
        // MAC mismatch is expected as our naive implementation does not persist
        // the correct MACs.
        Err(sgx_status_t::SGX_ERROR_MAC_MISMATCH) => (),
        _ => panic!("this should not happen"),
    }
}

#[cfg(not(feature = "sgx"))]
fn dummy_decrypt(ciphertext: &[u8], plaintext: &mut [u8]) {
    // TODO: do decryption
    plaintext.copy_from_slice(ciphertext);
}

#[cfg(test)]
mod test {
    use super::*;
    use block_device::mem_disk::MemDisk;

    fn test_setup() -> CryptDisk {
        let total_blocks = 3;
        let mem_disk = MemDisk::new(total_blocks).unwrap();
        let crypt_disk = CryptDisk::new(Box::new(mem_disk));
        crypt_disk
    }

    fn test_teardown(_disk: CryptDisk) {}

    block_device::gen_unit_tests!(test_setup, test_teardown);
}
