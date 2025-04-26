#!/bin/bash

for folder in results/* ; do 
    experiments+=(${folder#results/})
done

cargo run --release -- "${experiments[@]}" > output.log