use std::{
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex},
};

/// A bounded cache of reusable command buffers. Clones share the cache.
#[derive(Clone, Debug)]
pub struct BufferPool {
    inner: Arc<Mutex<Vec<Vec<u8>>>>,
    max_cached: usize,
    max_buffer: usize,
}

impl BufferPool {
    pub fn new(max_cached: usize, max_buffer: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Vec::with_capacity(max_cached))),
            max_cached,
            max_buffer,
        }
    }

    /// Own the buffer across task boundaries and return it to the pool on drop.
    pub fn acquire(&self, len: usize) -> PooledBuffer {
        let mut data = self
            .inner
            .lock()
            .ok()
            .and_then(|mut cache| {
                cache
                    .iter()
                    .position(|v| v.capacity() >= len)
                    .map(|i| cache.swap_remove(i))
            })
            .unwrap_or_default();
        data.resize(len, 0);
        PooledBuffer {
            data,
            pool: self.clone(),
        }
    }
}

/// Owned reusable buffer. Its contents remain valid until dropped.
#[derive(Debug)]
pub struct PooledBuffer {
    data: Vec<u8>,
    pool: BufferPool,
}

impl PooledBuffer {
    pub(crate) fn truncate(&mut self, len: usize) {
        self.data.truncate(len);
    }
}

impl Deref for PooledBuffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.data
    }
}

impl DerefMut for PooledBuffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}

impl Drop for PooledBuffer {
    fn drop(&mut self) {
        if self.data.capacity() <= self.pool.max_buffer
            && let Ok(mut cache) = self.pool.inner.lock()
            && cache.len() < self.pool.max_cached
        {
            self.data.clear();
            cache.push(std::mem::take(&mut self.data));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn buffer_is_recycled() {
        let pool = BufferPool::new(1, 1024);
        let mut b = pool.acquire(512);
        b[0] = 42;
        let ptr = b.as_ptr();
        drop(b);
        let b = pool.acquire(512);
        assert_eq!(ptr, b.as_ptr());
        assert_eq!(b[0], 0);
    }
}
