extern crate rustfft;
extern crate num;

use std::f64::consts::PI;
use std::collections::VecDeque;
use num::{Float, Complex, FromPrimitive, ToPrimitive};

#[allow(non_camel_case_types)]
type c64 = Complex<f64>;

/// Represents a component of the spectrum, composed of a frequency and amplitude.
#[derive(Copy, Clone)]
pub struct Bin {
    pub freq: f64,
    pub amp: f64,
}

impl Bin {
    pub fn new(freq: f64, amp: f64) -> Bin {
        Bin {
            freq: freq,
            amp: amp,
        }
    }
}

/// A phase vocoder.
///
/// Roughly translated from http://blogs.zynaptiq.com/bernsee/pitch-shifting-using-the-ft/
pub struct PhaseVocoder {
    channels: usize,
    sample_rate: f64,
    frame_size: usize,
    time_res: usize,

    samples_waiting: usize,
    in_buf: Vec<VecDeque<f64>>,
    out_buf: Vec<VecDeque<f64>>,
    last_phase: Vec<Vec<f64>>,
    sum_phase: Vec<Vec<f64>>,
    output_accum: Vec<VecDeque<f64>>,

    forward_fft: rustfft::FFT<f64>,
    backward_fft: rustfft::FFT<f64>,
}

impl PhaseVocoder {
    /// Constructs a new phase vocoder.
    ///
    /// `channels` is the number of channels of audio.
    ///
    /// `sample_rate` is the sample rate.
    ///
    /// `frame_size` is the fourier transform size. This should be a power of 2 for optimal
    /// performance. Will be rounded to a multiple of `time_res`.
    ///
    /// `time_res` is the number of frames to overlap.
    pub fn new(channels: usize,
               sample_rate: f64,
               frame_size: usize,
               time_res: usize)
               -> PhaseVocoder {
        let mut frame_size = frame_size / time_res * time_res;
        if frame_size == 0 {
            frame_size = time_res;
        }
        PhaseVocoder {
            channels: channels,
            sample_rate: sample_rate,
            frame_size: frame_size,
            time_res: time_res,

            samples_waiting: 0,
            in_buf: vec![VecDeque::new(); channels],
            out_buf: vec![VecDeque::new(); channels],
            last_phase: vec![vec![0.0; frame_size]; channels],
            sum_phase: vec![vec![0.0; frame_size]; channels],
            output_accum: vec![VecDeque::new(); channels],

            forward_fft: rustfft::FFT::new(frame_size, false),
            backward_fft: rustfft::FFT::new(frame_size, true),
        }
    }

    pub fn num_channels(&self) -> usize {
        self.channels
    }

    pub fn num_bins(&self) -> usize {
        self.frame_size
    }

    /// Reads samples from `input`, processes the samples, then resynthesizes as many samples as
    /// possible into `output`. Returns the number of samples written to `output`.
    ///
    /// `processor` is a function to manipulate the spectrum before it is resynthesized. Its
    /// arguments are respectively `num_channels`, `num_bins`, `analysis_output` and
    /// `synthesis_input`.
    ///
    /// Samples are expected to be normalized to the range [-1, 1].
    pub fn process<S, F>(&mut self,
                         input: &[&[S]],
                         output: &mut [&mut [S]],
                         mut processor: F)
                         -> usize
        where S: Float + ToPrimitive + FromPrimitive,
              F: FnMut(usize, usize, &[Vec<Bin>], &mut [Vec<Bin>])
    {
        assert_eq!(input.len(), self.channels);
        assert_eq!(output.len(), self.channels);

        // push samples to input queue
        for chan in 0..input.len() {
            for samp in 0..input[chan].len() {
                self.in_buf[chan].push_back(input[chan][samp].to_f64().unwrap());
                self.samples_waiting += 1;
            }
        }
        while self.samples_waiting >= 2 * self.frame_size * self.channels {
            let frame_sizef = self.frame_size as f64;
            let time_resf = self.time_res as f64;
            let step_size = frame_sizef / time_resf;
            let expect = 2.0 * PI * step_size / frame_sizef;
            let freq_per_bin = self.sample_rate / frame_sizef;
            let mut fft_in = vec![c64::new(0.0, 0.0); self.frame_size];
            let mut fft_out = vec![c64::new(0.0, 0.0); self.frame_size];

            for _ in 0..self.time_res {
                let mut analysis_out =
                    vec![vec![Bin::new(0.0, 0.0); self.frame_size]; self.channels];
                let mut synthesis_in =
                    vec![vec![Bin::new(0.0, 0.0); self.frame_size]; self.channels];

                // ANALYSIS
                for chan in 0..self.channels {
                    let samples = &self.in_buf[chan];
                    let mut last_phase = &mut self.last_phase[chan];

                    // read in
                    for i in 0..self.frame_size {
                        let window = window((i as f64) / frame_sizef);
                        fft_in[i] = c64::new(samples[i] * window, 0.0);
                    }

                    self.forward_fft.process(&fft_in, &mut fft_out);

                    for i in 0..self.frame_size {
                        let x = fft_out[i];

                        let (amp, phase) = x.to_polar();

                        // convert phase to frequency
                        let mut tmp = phase - last_phase[i];
                        last_phase[i] = phase;
                        tmp -= (i as f64) * expect;
                        let mut qpd = (tmp / PI) as i32;
                        if qpd >= 0 {
                            qpd += qpd & 1;
                        } else {
                            qpd -= qpd & 1;
                        }
                        tmp -= PI * (qpd as f64);
                        tmp = time_resf * tmp / (2.0 * PI);
                        tmp = (i as f64) * freq_per_bin + tmp * freq_per_bin;

                        analysis_out[chan][i] = Bin::new(tmp, amp * 2.0);
                    }
                }

                // PROCESSING
                processor(self.channels,
                          self.frame_size,
                          &analysis_out,
                          &mut synthesis_in);

                // SYNTHESIS
                for chan in 0..self.channels {
                    let mut sum_phase = &mut self.sum_phase[chan];
                    for i in 0..self.frame_size {
                        let amp = synthesis_in[chan][i].amp;
                        let mut tmp = synthesis_in[chan][i].freq;

                        // convert frequency to phase
                        tmp -= (i as f64) * freq_per_bin;
                        tmp /= freq_per_bin;
                        tmp = 2.0 * PI * tmp / time_resf;
                        tmp += (i as f64) * expect;
                        sum_phase[i] += tmp;
                        let phase = sum_phase[i];

                        fft_in[i] = c64::from_polar(&amp, &phase);
                    }

                    self.backward_fft.process(&fft_in, &mut fft_out);

                    // accumulate
                    for i in 0..self.frame_size {
                        let window = window((i as f64) / frame_sizef);
                        if i == self.output_accum[chan].len() {
                            self.output_accum[chan].push_back(0.0);
                        }
                        self.output_accum[chan][i] += window * fft_out[i].re /
                                                      (frame_sizef * time_resf);
                    }

                    // write out
                    for _ in 0..step_size as usize {
                        self.out_buf[chan].push_back(self.output_accum[chan].pop_front().unwrap());
                        self.in_buf[chan].pop_front();
                    }
                }
            }
            self.samples_waiting -= self.frame_size * self.channels;
        }

        // pop samples from output queue
        let mut n_written = 0;
        for chan in 0..self.channels {
            for samp in 0..output[chan].len() {
                output[chan][samp] = match self.out_buf[chan].pop_front() {
                    Some(x) => FromPrimitive::from_f64(x).unwrap(),
                    None => break,
                };
                n_written += 1;
            }
        }
        n_written / self.channels
    }
}

fn window(x: f64) -> f64 {
    -0.5 * (2.0 * PI * x).cos() + 0.5
}
