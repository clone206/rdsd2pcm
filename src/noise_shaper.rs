pub struct NoiseShaper {
    sos_count: usize,
    coeffs: Vec<f32>,
    state: Vec<f32>,
}

impl NoiseShaper {
    pub fn new(sos_count: usize, coeffs: &[f32]) -> Self {
        let state = vec![0.0; sos_count * 2]; // Assuming two states per section
        NoiseShaper {
            sos_count,
            coeffs: coeffs.to_vec(),
            state,
        }
    }

    pub fn update(&mut self, input: f32) {
        // Scale input to prevent clipping
        let scaled_input = input * 0.75; // Reduce input amplitude

        for i in 0..self.sos_count {
            let b1 = self.coeffs[i * 4];
            let b2 = self.coeffs[i * 4 + 1];
            let a1 = self.coeffs[i * 4 + 2];
            let a2 = self.coeffs[i * 4 + 3];

            let output = b1 * scaled_input + self.state[i * 2] * a1 + self.state[i * 2 + 1] * a2;
            self.state[i * 2 + 1] = self.state[i * 2]; // Shift state
            self.state[i * 2] = output; // Update state
        }
    }

    pub fn get(&self) -> f32 {
        // Scale output back to appropriate range
        self.state[0] * 0.25 // Reduce noise shaper influence
    }
}
