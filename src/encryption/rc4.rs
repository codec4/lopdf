// This module exists because the rust-crypto module is really old and not maintained.
// Fortunately the RC4 algorithm is very simple to implement.
pub struct Rc4 {
    initial_state: [u8; 256],
}

impl Rc4 {
    pub fn new<Key: AsRef<[u8]>>(key: Key) -> Self {
        let key = key.as_ref();
        assert!(!key.is_empty() && key.len() <= 256);

        let mut initial_state = [0_u8; 256];
        for (i, v) in initial_state.iter_mut().enumerate() {
            *v = i as u8;
        }

        let mut j = 0_u8;
        for i in 0..256 {
            j = j.wrapping_add(initial_state[i]).wrapping_add(key[i % key.len()]);
            initial_state.swap(i, j as usize);
        }

        Self { initial_state }
    }

    /// Encrypts/decrypts `input` into `output`.  The shorter of `input` and `output`
    ///  determine how many bytes are written into `output`.
    pub fn apply_keystream<'i, 'o, Input, Output>(&self, input: Input, output: Output)
    where
        Input: Iterator<Item = &'i u8>,
        Output: Iterator<Item = &'o mut u8>,
    {
        let mut keystream = self.keystream();
        for (i_byte, o_byte) in input.zip(output) {
            *o_byte = i_byte ^ keystream.next_byte();
        }
    }

    /// The keystream from its start, to apply a piece at a time.
    pub fn keystream(&self) -> Rc4Keystream {
        Rc4Keystream {
            state: self.initial_state,
            i: 0,
            j: 0,
        }
    }

    /// Allocates a new Vec<u8> of the same length as `input` and decrypts
    ///  `input` into it.
    pub fn decrypt<Input>(&self, input: Input) -> Vec<u8>
    where
        Input: AsRef<[u8]>,
    {
        let input = input.as_ref();
        let mut output = vec![0; input.len()];
        self.apply_keystream(input.iter(), output.iter_mut());
        output
    }

    /// Allocates a new Vec<u8> of the same length as `input` and encrypts
    ///  `input` into it.
    pub fn encrypt<Input>(&self, input: Input) -> Vec<u8>
    where
        Input: AsRef<[u8]>,
    {
        // Rc4 is symmetric
        self.decrypt(input)
    }
}

/// An RC4 keystream, continued from one piece of data to the next.
pub struct Rc4Keystream {
    state: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4Keystream {
    /// Encrypts or decrypts `data` in place.
    pub fn apply(&mut self, data: &mut [u8]) {
        for byte in data {
            *byte ^= self.next_byte();
        }
    }

    fn next_byte(&mut self) -> u8 {
        self.i = self.i.wrapping_add(1);
        self.j = self.j.wrapping_add(self.state[self.i as usize]);
        self.state.swap(self.i as usize, self.j as usize);
        self.state[(self.state[self.i as usize].wrapping_add(self.state[self.j as usize])) as usize]
    }
}
