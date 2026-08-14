# Models

`slik` currently performs object detection with NanoDet. Each model must be
paired with a model-info file that describes its input and output tensor
contract.

## Default model

The default setup task downloads and verifies the NanoDet-Plus-m 320x320 model:

```sh
mise run setup:model
```

The matching pair is installed at:

```text
crates/slik/models/nanodet-plus-m-320.onnx
crates/slik/models/nanodet-plus-m-320.onnx.modelinfo
```

Model binaries are ignored by Git. Downloads are verified against pinned
SHA-256 hashes.

## Alternative fixture

Provision and select the NanoDet-Plus-m 416x416 fixture with:

```sh
mise run setup:model:nanodet-plus-m-416
mise exec -- cargo run --locked --release -p slik -- \
  --model crates/slik/models/nanodet-plus-m-416.onnx \
  --model-info crates/slik/models/nanodet-plus-m-416.onnx.modelinfo
```

## Supported contracts

The current NanoDet decoder supports these output tensors:

| Model | Output tensor |
| --- | --- |
| NanoDet-m 320 | `[1, 2100, 112]` |
| NanoDet-Plus 320 | `[1, 2125, 112]` |
| NanoDet-m 416 | `[1, 3549, 112]` |
| NanoDet-Plus 416 | `[1, 3598, 112]` |

Only the NanoDet-Plus-m 320x320 and 416x416 fixtures have provisioning tasks.
For another supported model, pass its matching files with `--model` and
`--model-info`. Use `--labels` to select a different label file.
