use core::ptr;

use embassy_embedded_hal::SetConfig;
#[cfg(not(gpdma))]
use embassy_futures::join::join;
use embassy_hal_internal::{into_ref, PeripheralRef};
use embedded_hal_02::spi::{Mode, Phase, Polarity, MODE_0};
use embedded_hal_nb::nb;

#[cfg(not(gpdma))]
use super::{check_error_flags, set_rxdmaen, set_txdmaen, RxDma, TxDma};
use super::{
    rx_ready, tx_ready, word_impl, BitOrder, CsPin, Error, Instance, MisoPin, MosiPin, RegsExt, SckPin, SealedWord,
    Word,
};
#[cfg(not(gpdma))]
use crate::dma::{Priority, ReadableRingBuffer, TransferOptions, WritableRingBuffer};
use crate::gpio::{AFType, AnyPin, SealedPin as _};
use crate::pac::spi::{vals, Spi as Regs};
use crate::Peripheral;

/// SPI slave configuration.
#[non_exhaustive]
#[derive(Copy, Clone)]
pub struct Config {
    /// SPI mode.
    pub mode: Mode,
    /// Bit order.
    pub bit_order: BitOrder,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: MODE_0,
            bit_order: BitOrder::MsbFirst,
        }
    }
}

impl Config {
    fn raw_phase(&self) -> vals::Cpha {
        match self.mode.phase {
            Phase::CaptureOnSecondTransition => vals::Cpha::SECONDEDGE,
            Phase::CaptureOnFirstTransition => vals::Cpha::FIRSTEDGE,
        }
    }

    fn raw_polarity(&self) -> vals::Cpol {
        match self.mode.polarity {
            Polarity::IdleHigh => vals::Cpol::IDLEHIGH,
            Polarity::IdleLow => vals::Cpol::IDLELOW,
        }
    }

    fn raw_byte_order(&self) -> vals::Lsbfirst {
        match self.bit_order {
            BitOrder::LsbFirst => vals::Lsbfirst::LSBFIRST,
            BitOrder::MsbFirst => vals::Lsbfirst::MSBFIRST,
        }
    }
}

/// SPI slave driver.
///
/// This driver provides blocking software-driven read and write methods. The driver can be turned
/// into an asynchronous one by providing DMA peripherals using `Self::dma_ringbuffered`.
///
/// For SPI buses with high-frequency clocks you must use the asynchronous driver, as the chip is
/// not fast enough to drive the SPI in software.
pub struct SpiSlave<'d, T: Instance> {
    _peri: PeripheralRef<'d, T>,
    sck: Option<PeripheralRef<'d, AnyPin>>,
    mosi: Option<PeripheralRef<'d, AnyPin>>,
    miso: Option<PeripheralRef<'d, AnyPin>>,
    cs: Option<PeripheralRef<'d, AnyPin>>,
    current_word_size: word_impl::Config,
}

#[cfg(not(gpdma))]
pub struct SpiSlaveRingBuffered<'d, T: Instance, W: Word> {
    _inner: SpiSlave<'d, T>,
    tx_ring_buffer: WritableRingBuffer<'d, W>,
    rx_ring_buffer: ReadableRingBuffer<'d, W>,
}

impl<'d, T: Instance> SpiSlave<'d, T> {
    /// Create a new SPI slave driver.
    pub fn new<Cs>(
        peri: impl Peripheral<P = T> + 'd,
        sck: impl Peripheral<P = impl SckPin<T>> + 'd,
        mosi: impl Peripheral<P = impl MosiPin<T>> + 'd,
        miso: impl Peripheral<P = impl MisoPin<T>> + 'd,
        cs: impl Peripheral<P = Cs> + 'd,
        config: Config,
    ) -> Self
    where
        Cs: CsPin<T>,
    {
        into_ref!(peri, sck, mosi, miso, cs);

        sck.set_as_af(sck.af_num(), AFType::Input);
        sck.set_speed(crate::gpio::Speed::VeryHigh);
        mosi.set_as_af(mosi.af_num(), AFType::Input);
        mosi.set_speed(crate::gpio::Speed::VeryHigh);
        miso.set_as_af(miso.af_num(), AFType::OutputPushPull);
        miso.set_speed(crate::gpio::Speed::VeryHigh);
        cs.set_as_af(cs.af_num(), AFType::Input);
        cs.set_speed(crate::gpio::Speed::VeryHigh);

        Self::new_inner(
            peri,
            Some(sck.map_into()),
            Some(mosi.map_into()),
            Some(miso.map_into()),
            Some(cs.map_into()),
            config,
        )
    }

    fn new_inner(
        peri: impl Peripheral<P = T> + 'd,
        sck: Option<PeripheralRef<'d, AnyPin>>,
        mosi: Option<PeripheralRef<'d, AnyPin>>,
        miso: Option<PeripheralRef<'d, AnyPin>>,
        cs: Option<PeripheralRef<'d, AnyPin>>,
        config: Config,
    ) -> Self {
        into_ref!(peri);

        let cpha = config.raw_phase();
        let cpol = config.raw_polarity();

        let lsbfirst = config.raw_byte_order();

        T::enable_and_reset();

        #[cfg(any(spi_v1, spi_f1))]
        {
            T::REGS.cr1().modify(|w| {
                w.set_cpha(cpha);
                w.set_cpol(cpol);

                w.set_mstr(vals::Mstr::SLAVE);
                w.set_ssm(false);

                w.set_lsbfirst(lsbfirst);
                w.set_crcen(false);
                w.set_bidimode(vals::Bidimode::UNIDIRECTIONAL);
                if miso.is_none() {
                    w.set_rxonly(vals::Rxonly::OUTPUTDISABLED);
                }
                w.set_dff(<u8 as SealedWord>::CONFIG)
            });
        }
        #[cfg(spi_v2)]
        {
            T::REGS.cr2().modify(|w| {
                let (ds, frxth) = <u8 as SealedWord>::CONFIG;
                w.set_frxth(frxth);
                w.set_ds(ds);
            });
            T::REGS.cr1().modify(|w| {
                w.set_cpha(cpha);
                w.set_cpol(cpol);

                w.set_mstr(vals::Mstr::SLAVE);
                w.set_ssm(false);

                w.set_lsbfirst(lsbfirst);
                w.set_crcen(false);
                w.set_bidimode(vals::Bidimode::UNIDIRECTIONAL);
            });
        }
        #[cfg(any(spi_v3, spi_v4, spi_v5))]
        {
            T::REGS.ifcr().write(|w| w.0 = 0xffff_ffff);
            T::REGS.cfg2().modify(|w| {
                w.set_cpha(cpha);
                w.set_cpol(cpol);
                w.set_lsbfirst(lsbfirst);

                w.set_master(vals::Master::SLAVE);
                w.set_ssm(false);

                w.set_comm(vals::Comm::FULLDUPLEX);
                w.set_ssom(vals::Ssom::ASSERTED);
                w.set_midi(0);
                w.set_mssi(0);
                w.set_afcntr(true);
                w.set_ssiop(vals::Ssiop::ACTIVEHIGH);
            });
            T::REGS.cfg1().modify(|w| {
                w.set_crcen(false);
                w.set_dsize(<u8 as SealedWord>::CONFIG);
                w.set_fthlv(vals::Fthlv::ONEFRAME);
            });
            T::REGS.cr2().modify(|w| {
                w.set_tsize(0);
            });
            T::REGS.cr1().modify(|w| {
                w.set_ssi(false);
            });
        }

        Self {
            _peri: peri,
            sck,
            mosi,
            miso,
            cs,
            current_word_size: <u8 as SealedWord>::CONFIG,
        }
    }

    /// Set the GPIO speed for the MISO pin.
    pub fn set_miso_speed(&mut self, speed: crate::gpio::Speed) {
        self.miso.as_ref().map(|x| x.set_speed(speed));
    }

    fn set_word_size(&mut self, word_size: word_impl::Config) {
        if self.current_word_size == word_size {
            return;
        }

        #[cfg(any(spi_v1, spi_f1))]
        {
            T::REGS.cr1().modify(|reg| {
                reg.set_spe(false);
                reg.set_dff(word_size)
            });
            T::REGS.cr1().modify(|reg| {
                reg.set_spe(true);
            });
        }
        #[cfg(spi_v2)]
        {
            T::REGS.cr1().modify(|w| {
                w.set_spe(false);
            });
            T::REGS.cr2().modify(|w| {
                w.set_frxth(word_size.1);
                w.set_ds(word_size.0);
            });
            T::REGS.cr1().modify(|w| {
                w.set_spe(true);
            });
        }
        #[cfg(any(spi_v3, spi_v4, spi_v5))]
        {
            T::REGS.cr1().modify(|w| {
                w.set_csusp(true);
            });
            while T::REGS.sr().read().eot() {}
            T::REGS.cr1().modify(|w| {
                w.set_spe(false);
            });
            T::REGS.cfg1().modify(|w| {
                w.set_dsize(word_size);
            });
            T::REGS.cr1().modify(|w| {
                w.set_csusp(false);
                w.set_spe(true);
            });
        }

        self.current_word_size = word_size;
    }

    /// Turn the SPI driver into an asynchronous driver using ring buffer-backed DMA.
    #[cfg(not(gpdma))]
    pub fn dma_ringbuffered<'b, Tx, Rx, W: Word>(
        mut self,
        txdma: impl Peripheral<P = Tx> + 'd,
        rxdma: impl Peripheral<P = Rx> + 'd,
        txdma_buffer: &'b mut [W],
        rxdma_buffer: &'b mut [W],
    ) -> SpiSlaveRingBuffered<'b, T, W>
    where
        'd: 'b,
        Tx: TxDma<T>,
        Rx: RxDma<T>,
    {
        into_ref!(txdma, rxdma);

        self.set_word_size(W::CONFIG);
        T::REGS.cr1().modify(|w| w.set_spe(false));

        // The reference manual says to set RXDMAEN, configure streams, set TXDMAEN, enable SPE, in
        // that order.
        set_rxdmaen(T::REGS, true);

        let mut opts = TransferOptions::default();
        opts.half_transfer_ir = true;
        opts.priority = Priority::High;
        let rx_request = rxdma.request();
        let rx_src = T::REGS.rx_ptr();
        let mut rx_ring_buffer = unsafe { ReadableRingBuffer::new(rxdma, rx_request, rx_src, rxdma_buffer, opts) };

        let mut opts = TransferOptions::default();
        opts.priority = Priority::VeryHigh;
        let tx_request = txdma.request();
        let tx_src = T::REGS.tx_ptr();
        let mut tx_ring_buffer = unsafe { WritableRingBuffer::new(txdma, tx_request, tx_src, txdma_buffer, opts) };

        set_txdmaen(T::REGS, true);

        T::REGS.cr1().modify(|w| w.set_spe(true));

        rx_ring_buffer.start();
        tx_ring_buffer.start();

        SpiSlaveRingBuffered {
            _inner: self,
            tx_ring_buffer,
            rx_ring_buffer,
        }
    }

    /// Write a word to the SPI.
    pub fn write<W: Word>(&mut self, word: W) -> nb::Result<(), Error> {
        T::REGS.cr1().modify(|w| w.set_spe(true));
        self.set_word_size(W::CONFIG);

        let _ = transfer_word(T::REGS, word)?;

        Ok(())
    }

    /// Read a word from the SPI.
    pub fn read<W: Word>(&mut self) -> nb::Result<W, Error> {
        T::REGS.cr1().modify(|w| w.set_spe(true));
        self.set_word_size(W::CONFIG);

        transfer_word(T::REGS, W::default())
    }

    /// Bidirectionally transfer by writing a word to SPI while simultaneously reading a word from
    /// the SPI during the same clock cycle.
    pub fn transfer<W: Word>(&mut self, word: W) -> nb::Result<W, Error> {
        T::REGS.cr1().modify(|w| w.set_spe(true));
        self.set_word_size(W::CONFIG);

        transfer_word(T::REGS, word)
    }
}

#[cfg(not(gpdma))]
impl<'d, T: Instance, W> SpiSlaveRingBuffered<'d, T, W>
where
    W: Word,
{
    /// Write elements from `buf` into the transmit ringbuffer. These elements will be transmitted
    /// over SPI in the background using DMA.
    ///
    /// The number of elements that were read from `buf` is returned. An overrun error occurs when
    /// the portion to be written to was read by DMA.
    pub fn write(&mut self, buf: &[W]) -> Result<usize, Error> {
        // `WritableRingBuffer` errors with Overrun if we try to send an empty buffer
        if buf.is_empty() {
            return Ok(0);
        }

        match self.tx_ring_buffer.write(buf) {
            Ok((written, _)) => Ok(written),
            Err(_) => {
                self.tx_ring_buffer.clear();
                Err(Error::Overrun)
            }
        }
    }

    /// Seek the buffer to a specific offset from the current position.
    pub fn seek(&mut self, offset: usize) -> usize {
        self.tx_ring_buffer.seek(offset)
    }

    /// Read elements from the receive ringbuffer into `buf`. Elements received over SPI are
    /// written into the receive ringbuffer in the background using DMA.
    ///
    /// The number of elements that were written into `buf` is returned. An overrun error occurs
    /// when the portion to be read was overwritten by DMA.
    pub fn read(&mut self, buf: &mut [W]) -> Result<usize, Error> {
        match self.rx_ring_buffer.read(buf) {
            Ok((read, _)) => Ok(read),
            Err(_) => {
                self.rx_ring_buffer.clear();
                Err(Error::Overrun)
            }
        }
    }

    /// Read an exact number of elements from the receive ringbuffer into `buf`.
    ///
    /// An overrun error occurs when the portion to be read was overwritten by DMA.
    pub async fn read_exact(&mut self, buf: &mut [W]) -> Result<(), Error> {
        self.rx_ring_buffer.read_exact(buf).await.map_err(|_| Error::Overrun)?;

        let sr = T::REGS.sr().read();
        check_error_flags(sr)?;

        Ok(())
    }

    /// Write an exact number of elements from `buf` to the transmit ringbuffer.
    ///
    /// An overrun error occurs when the portion to be written was read by DMA.
    pub async fn write_exact(&mut self, buf: &[W]) -> Result<(), Error> {
        self.tx_ring_buffer.write_exact(buf).await.map_err(|_| Error::Overrun)?;

        let sr = T::REGS.sr().read();
        check_error_flags(sr)?;

        Ok(())
    }

    /// Write all elements from `write_buf` into the transmit ringbuffer and read exactly
    /// `read_buf.len()` elements into `read_buf` from the receive ringbuffer.
    ///
    /// An overrun error occurs when either the portion to be written to was read by DMA or the
    /// portion to be read was written to by DMA.
    pub async fn transfer_exact(&mut self, write_buf: &[W], read_buf: &mut [W]) -> Result<(), Error> {
        let write = self.tx_ring_buffer.write_exact(write_buf);
        let read = self.rx_ring_buffer.read_exact(read_buf);

        let result = join(write, read).await;
        result.0.map_err(|_| Error::Overrun)?;
        result.1.map_err(|_| Error::Overrun)?;

        let sr = T::REGS.sr().read();
        check_error_flags(sr)?;

        Ok(())
    }
}

impl<'d, T: Instance> Drop for SpiSlave<'d, T> {
    fn drop(&mut self) {
        self.sck.as_ref().map(|x| x.set_as_disconnected());
        self.mosi.as_ref().map(|x| x.set_as_disconnected());
        self.miso.as_ref().map(|x| x.set_as_disconnected());
        self.cs.as_ref().map(|x| x.set_as_disconnected());

        T::disable();
    }
}

impl<'d, T: Instance> SetConfig for SpiSlave<'d, T> {
    type Config = Config;
    type ConfigError = ();
    fn set_config(&mut self, _config: &Self::Config) -> Result<(), ()> {
        unimplemented!()
    }
}

fn transfer_word<W: Word>(regs: Regs, tx_word: W) -> nb::Result<W, Error> {
    // To keep the tx and rx FIFO queues in the SPI peripheral synchronized, a word must be
    // simultaneously sent and received, even when only sending or receiving.
    if !tx_ready(regs)? || !rx_ready(regs)? {
        return Err(nb::Error::WouldBlock);
    }

    unsafe {
        ptr::write_volatile(regs.tx_ptr(), tx_word);

        #[cfg(any(spi_v3, spi_v4, spi_v5))]
        regs.cr1().modify(|reg| reg.set_cstart(true));
    }

    let rx_word = unsafe { ptr::read_volatile(regs.rx_ptr()) };
    Ok(rx_word)
}
