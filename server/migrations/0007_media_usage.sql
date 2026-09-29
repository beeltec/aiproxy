-- Usage of the media endpoints and of the image-generation tool.
ALTER TABLE usage ADD COLUMN images_generated INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage ADD COLUMN image_size TEXT;
ALTER TABLE usage ADD COLUMN image_quality TEXT;
-- Images in the request, counted by the gateway (OpenRouter prices input images).
ALTER TABLE usage ADD COLUMN input_images INTEGER NOT NULL DEFAULT 0;
-- Characters of speech input, and seconds of transcribed audio.
ALTER TABLE usage ADD COLUMN characters INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage ADD COLUMN seconds REAL;
