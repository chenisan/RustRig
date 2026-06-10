//! 雙工交接用的 lock-free SPSC ring。
//!
//! capture 執行緒是唯一 producer，render 執行緒是唯一 consumer。ring 的
//! 水位（fill level）**就是** drift / jitter 緩衝：太空→underrun（爆音），
//! 太滿→latency 爬升。後端會監看水位來偵測 xrun。

use rtrb::{Consumer, Producer, RingBuffer};

/// 建立一條單聲道樣本 ring。
///
/// `capacity` 建議取 `block_size * 數倍`，預留 drift 緩衝空間。
pub fn channel(capacity: usize) -> (Producer<f32>, Consumer<f32>) {
    RingBuffer::new(capacity)
}

/// 從 ring 取出剛好 `out.len()` 個樣本。不足的部分填 0（underrun）。
///
/// 回傳實際取到的樣本數；`out.len() - 取到數` 即本次補了多少靜音。
#[inline]
pub fn pop_fill(consumer: &mut Consumer<f32>, out: &mut [f32]) -> usize {
    let mut n = 0;
    while n < out.len() {
        match consumer.pop() {
            Ok(s) => {
                out[n] = s;
                n += 1;
            }
            Err(_) => break, // ring 空了：剩下的留給下面補 0
        }
    }
    for s in &mut out[n..] {
        *s = 0.0;
    }
    n
}

/// 把 `data` 全部塞進 ring。回傳成功塞入數；`data.len() - 塞入數` 即 overflow 丟棄量。
#[inline]
pub fn push_all(producer: &mut Producer<f32>, data: &[f32]) -> usize {
    let mut n = 0;
    for &s in data {
        if producer.push(s).is_err() {
            break; // ring 滿了：overflow
        }
        n += 1;
    }
    n
}
