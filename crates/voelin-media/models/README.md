# Models

`pphumanseg-2023mar.onnx`: PP-HumanSeg, PaddleSeg's portrait segmentation
model, as OpenCV Zoo ships it
(`models/human_segmentation_pphumanseg/human_segmentation_pphumanseg_2023mar.onnx`
in https://github.com/opencv/opencv_zoo, ported from PaddleHub). Copyright (c)
2021 PaddlePaddle Authors, under the Apache License 2.0
(`LICENSE-PP-HumanSeg`). Unchanged; SHA-256
`552d8a984054e59b5d773d24b9b12022b22046ceb2bbc4c9aaeaceb36a9ddf24`.

Input `x`: 1x3x192x192 RGB, `(value / 255 - 0.5) / 0.5`. Output: 1x2x192x192
probabilities, channel 1 the person. The studio's background replacement
(`src/studio/segment.rs`, feature `segment`) runs it with tract.
