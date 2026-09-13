# Embedded embedding model

`semantic_search` runs the model in this directory **in-process, offline**: the
files are compiled into the `comrade-tool-memory` binary with `include_bytes!`
(see `src/semantic.rs`). There is no download and no model cache.

These files are the **source of truth**. At build time `build.rs` deflates each
one with flate2 into `OUT_DIR/assets/<name>.deflate`, and the binary embeds those
compressed copies and inflates them in memory on first model use. The ~35 MB of
raw assets therefore cost ~25 MB in the binary; the release profile (`strip =
true`) removes a further ~14 MB of symbol tables.

## Contents

| file | bytes | what |
| --- | --- | --- |
| `model_quantized.onnx` | 34,014,426 | int8 dynamic-quantized ONNX graph |
| `tokenizer.json` | 711,396 | HuggingFace fast tokenizer |
| `config.json` | 683 | tokenizer/model config |
| `tokenizer_config.json` | 366 | tokenizer config |
| `special_tokens_map.json` | 125 | special tokens |

Total: ~35 MB raw, ~25 MB in the binary after build-time deflate.

## Model

- **BAAI/bge-small-en-v1.5** (33.4M params, 384-dim, English), CLS pooling.
- int8 dynamic quantization of the Xenova conversion
  (`Xenova/bge-small-en-v1.5`, `onnx/model_quantized.onnx`), which is the ONNX
  form of BAAI's model.
- Embedding is produced by `fastembed` via
  `TextEmbedding::try_new_from_user_defined` with `Pooling::Cls`.

Size reference: fp32 `model.onnx` is 133 MB, the fp16 `model_optimized.onnx`
fastembed downloads by default is 66 MB, this int8 file is 34 MB — and it is
embedded, so nothing is fetched at runtime.

## License

The model is published by the Beijing Academy of Artificial Intelligence (BAAI)
under the **MIT License**; the ONNX conversion by Xenova is likewise MIT. The
tokenizer/config files come from the same conversion. Keep this notice with the
files if they are redistributed.

## Refreshing the assets

```sh
B=https://huggingface.co/Xenova/bge-small-en-v1.5/resolve/main
curl -sL -o model_quantized.onnx "$B/onnx/model_quantized.onnx"
curl -sL -o tokenizer.json "$B/tokenizer.json"
curl -sL -o config.json "$B/config.json"
curl -sL -o tokenizer_config.json "$B/tokenizer_config.json"
curl -sL -o special_tokens_map.json "$B/special_tokens_map.json"
```

If the model changes, bump `MODEL_ID` in `src/semantic.rs` so existing vector
indexes are invalidated and rebuilt.
