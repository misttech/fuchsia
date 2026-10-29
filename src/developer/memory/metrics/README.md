Bucket Matcher Configuration
==========================

Bucket matcher configuration can be specified using a JSON file at
`/config/data/buckets.json`.

The `buckets.json` should contain a JSON array of bucket matcher objects.
Each bucket matcher must have three members:
* `name`: the name of the reported bucket in the memory monitor output.
* `process`: an array of literal string prefixes matching the name of all
processes whose VMOs should be considered for this bucket (an empty array `[]`
matches all processes).
* `vmo`: an array of literal string prefixes matching the name of VMOs that
should be considered for this bucket (an empty array `[]` matches all VMOs).
