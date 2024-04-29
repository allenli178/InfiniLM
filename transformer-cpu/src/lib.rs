mod kernel;

use causal_lm::{CausalLM, DecodingMeta, Model, QueryContext, SampleMeta};
use common::{upos, utok, Blob, FileLoadError};
use gemm::f16;
use itertools::izip;
use kernel::{
    fused_softmax::softmax, gather::gather, mat_mul::mat_mul, rms_norm::rms_norm,
    rotary_embedding::rotary_embedding, swiglu::swiglu,
};
use llama::Storage;
use std::{iter::repeat, path::Path, slice::from_raw_parts};
use tensor::{reslice, slice, split, udim, LocalSplitable, Tensor};

pub struct Transformer(Storage);

impl Model for Transformer {
    type Meta = ();
    type Error = FileLoadError;

    #[inline]
    fn load(model_dir: impl AsRef<Path>, _meta: Self::Meta) -> Result<Self, Self::Error> {
        Ok(Self(llama::Storage::load_safetensors(model_dir)?))
    }
}

impl CausalLM for Transformer {
    type Storage = Blob;

    #[inline]
    fn eos_token(&self) -> utok {
        self.0.config.eos_token
    }

    fn new_cache(&self) -> Tensor<Self::Storage> {
        let dt = self.0.config.dt;
        let nlayers = self.0.config.nlayers;
        let nkvh = self.0.config.nkvh;
        let max_seq_len = self.0.config.max_seq_len;
        let d = self.0.config.d;
        let nh = self.0.config.nh;
        Tensor::alloc(dt, &[nlayers, 2, nkvh, max_seq_len, d / nh], Blob::new)
    }

    fn duplicate_cache(&self, cache: &Tensor<Self::Storage>, pos: upos) -> Tensor<Self::Storage> {
        let &[_nlayers, 2, _nkvh, max_seq_len, _dh] = cache.shape() else {
            panic!()
        };
        assert!(pos <= max_seq_len);
        let slice = [
            slice![=>],
            slice![=>],
            slice![=>],
            slice![=>pos],
            slice![=>],
        ];

        let mut ans = Tensor::alloc(cache.data_type(), cache.shape(), Blob::new);
        cache
            .as_ref()
            .slice(&slice)
            .map_physical(|u| &**u)
            .reform_to(&mut ans.as_mut().slice(&slice).map_physical(|u| &mut **u));
        ans
    }

    fn token_embed(&self, queries: impl IntoIterator<Item = utok>) -> Tensor<Self::Storage> {
        let dt = self.0.config.dt;
        let d = self.0.config.d;

        let tokens = queries.into_iter().collect::<Vec<_>>();
        let nt = tokens.len() as udim;

        let mut x = Tensor::alloc(dt, &[nt, d], Blob::new);
        gather(&mut x, &self.0.embed_tokens, tokens);
        x
    }

    fn forward<'a>(
        &self,
        queries: impl IntoIterator<Item = QueryContext<'a, Self::Storage>>,
        token_embedded: Tensor<Self::Storage>,
    ) -> Tensor<Self::Storage>
    where
        Self: 'a,
    {
        let mut queries = queries.into_iter().collect::<Vec<_>>();
        let mut nt = 0;
        let mut max_seq_len = 0;
        let mut max_att_len = 0;
        let seq_len = queries
            .iter()
            .map(|q| {
                let seq = q.seq_len();
                let att = q.att_len();
                nt += seq;
                max_seq_len = max_seq_len.max(seq);
                max_att_len = max_att_len.max(att);
                seq
            })
            .collect::<Vec<_>>();

        let dt = self.0.config.dt;
        let d = self.0.config.d;
        let nh = self.0.config.nh;
        let nkvh = self.0.config.nkvh;
        let dh = d / nh;
        let dkv = nkvh * dh;
        let di = self.0.config.di;
        let head_group = nh / nkvh;
        let head_div = (dh as f32).sqrt().recip();

        let reusing = (d + dkv + dkv).max(di + di);
        let mut state_buf = Tensor::alloc(dt, &[nt, d + reusing], Blob::new);
        macro_rules! state {
            () => {
                split!(state_buf.as_mut().map_physical(|u| LocalSplitable::from(&mut **u)); [1]: d, reusing)
            };
        }

        let mut q_buf = Blob::new((nh * max_seq_len * dh) as usize * dt.size());
        let mut att_buf = Blob::new((nh * max_seq_len * max_att_len) as usize * dt.size());
        let pos = causal_lm::pos(&queries, nt);
        let pos = pos.as_ref().map_physical(|u| reslice(u));

        let mut x = token_embedded;
        for (layer, params) in self.0.layers.iter().enumerate() {
            let (mut x1, qkv) = state!();
            let mut qkv = qkv.slice(&[slice![=>], slice![=> d + dkv + dkv]]);

            rms_norm(&mut x1, &x, &params.att_layernorm, self.0.config.epsilon);
            mat_mul(&mut qkv, 0., &x1, &params.att_qkv, 1.);

            let (q, k, v) = split!(qkv; [1]: d, dkv, dkv);
            let mut q = q.reshape(&[nt, nh, dh]);
            let mut k = k.reshape(&[nt, nkvh, dh]);
            let v = v.reshape(&[nt, nkvh, dh]);
            let o = x1.reshape(&[nt, nh, dh]);

            rotary_embedding(&mut q, &pos, self.0.config.theta);
            rotary_embedding(&mut k, &pos, self.0.config.theta);

            let q = q.transpose(&[1, 0, 2]).split(1, &seq_len);
            let k = k.transpose(&[1, 0, 2]).split(1, &seq_len);
            let v = v.transpose(&[1, 0, 2]).split(1, &seq_len);
            let o = o.transpose(&[1, 0, 2]).split(1, &seq_len);

            for (query, q, k, v, mut o) in izip!(&mut queries, q, k, v, o) {
                let pos = query.pos();
                let seq_len = query.seq_len();
                let att_len = query.att_len();
                let Some((mut k_cache, mut v_cache)) = query.cache(layer as _) else {
                    continue;
                };

                let slice_cat = &[slice![=>], slice![pos =>=> seq_len], slice![=>]];
                let slice_att = &[slice![=>], slice![      => att_len], slice![=>]];
                let shape_q0 = &[nkvh * head_group, seq_len, dh];
                let shape_q1 = &[nkvh, head_group * seq_len, dh];
                let shape_att0 = &[nkvh, head_group * seq_len, att_len];
                let shape_att1 = &[nkvh * head_group, seq_len, att_len];

                let mut q_att = Tensor::new(dt, shape_q0, &mut q_buf[..]);
                let mut k_cat = k_cache.as_mut().slice(slice_cat).map_physical(|u| &mut **u);
                let mut v_cat = v_cache.as_mut().slice(slice_cat).map_physical(|u| &mut **u);
                q.reform_to(&mut q_att);
                k.reform_to(&mut k_cat);
                v.reform_to(&mut v_cat);

                let q_att = q_att.reshape(shape_q1);
                let k_att = k_cache.slice(slice_att).transpose(&[0, 2, 1]);
                let v_att = v_cache.slice(slice_att);

                let mut att = Tensor::new(dt, shape_att0, &mut att_buf[..]);
                mat_mul(&mut att, 0., &q_att, &k_att, head_div);
                let mut att = att.reshape(shape_att1);
                softmax(&mut att);
                let mut x2 = q_att;
                mat_mul(&mut x2, 0., &att.reshape(shape_att0), &v_att, 1.);

                x2.reshape(shape_q0).reform_to(&mut o);
            }

            let (mut x1, gate_up) = state!();
            let mut gate_up = gate_up.slice(&[slice![=>], slice![=> di + di]]);

            mat_mul(&mut x, 1., &x1, &params.att_o, 1.);
            rms_norm(&mut x1, &x, &params.mlp_layernorm, self.0.config.epsilon);
            mat_mul(&mut gate_up, 0., &x1, &params.mlp_gate_up, 1.);
            let (mut gate, up) = split!(gate_up; [1]: di, di);
            swiglu(&mut gate, &up);
            mat_mul(&mut x, 1., &gate, &params.mlp_down, 1.);
        }

        x
    }

    fn decode(
        &self,
        decoding: impl IntoIterator<Item = DecodingMeta>,
        mut hidden_state: Tensor<Self::Storage>,
    ) -> Tensor<Self::Storage> {
        let dt = self.0.config.dt;
        let d = self.0.config.d;

        let buf = hidden_state.as_mut_slice();
        let len = d as usize * dt.size();

        let mut iter = decoding.into_iter();
        let mut begin = 0;
        let mut src = 0;
        let mut dst = 0;
        for DecodingMeta {
            num_query,
            num_decode,
        } in iter.by_ref()
        {
            begin += num_query;
            if num_decode > 0 {
                src = begin;
                dst = begin;
                begin -= num_decode;
                break;
            }
        }
        for DecodingMeta {
            num_query,
            num_decode,
        } in iter
        {
            src += num_query - num_decode;
            if src > dst {
                for _ in 0..num_decode {
                    buf.copy_within(src * len..(src + 1) * len, dst * len);
                    src += 1;
                    dst += 1;
                }
            } else {
                src += num_decode;
                dst += num_decode;
            }
        }

        if dst == begin {
            return Tensor::alloc(dt, &[0, d as _], Blob::new);
        }

        let lm_head = &self.0.lm_head;
        let mut x = hidden_state.slice(&[slice![begin => dst], slice![=>]]);
        let mut logits = Tensor::alloc(dt, &[x.shape()[0], lm_head.shape()[1]], Blob::new);

        // 复制一个 x 以实现原地归一化
        let x_ = x
            .as_ref()
            .map_physical(|u| unsafe { from_raw_parts(u.as_ptr(), u.len()) });
        rms_norm(&mut x, &x_, &self.0.lm_layernorm, self.0.config.epsilon);
        mat_mul(&mut logits, 0., &x, lm_head, 1.);

        logits
    }

    fn sample(
        &self,
        args: impl IntoIterator<Item = SampleMeta>,
        logits: Tensor<Self::Storage>,
    ) -> Vec<utok> {
        let &[_, voc] = logits.shape() else { panic!() };
        let logits: &[f16] = reslice(logits.as_slice());
        args.into_iter()
            .flat_map(|meta| repeat(meta.args).take(meta.num_decode))
            .enumerate()
            .map(|(i, args)| args.random(&kernel::slice!(logits; voc; [i])))
            .collect()
    }
}

#[test]
fn test_infer() {
    causal_lm::test_impl::<Transformer>(
        (),
        &[
            29966, 29989, 1792, 29989, 29958, 13, 29903, 388, 376, 18567, 29908, 304, 592, 21106,
            29879, 5299, 29989, 465, 22137, 29989, 29958, 13,
        ],
    );
}
