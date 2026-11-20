---
name: dataviz
description: "Build charts, dashboards, and plotting code in any library: choose a fitting form, encode data honestly, pick accessible colors, and verify the rendered result."
user-invocable: true
disable-model-invocation: false
---
Turn the request into a data visualization.

Begin with the question the chart has to answer, not with the chart type or the colors. Write the comparison down in one sentence, then choose the form that makes that comparison easiest to read.

## Pick the form from the question

- **Comparison between categories** — bars. Use horizontal bars when the labels are long or numerous, vertical bars when they are short, and sort by value unless the categories have an inherent order.
- **Change over time** — lines for continuous series, bars for discrete periods. Keep the time spacing consistent and do not connect across missing periods.
- **Distribution** — a histogram for one group, box plots or strip plots for several. Show individual points when the sample is small enough for them to be informative.
- **Relationship between two measures** — a scatter plot. Add a trend line only when the relationship itself is the message, and say what the line is fitted to.
- **Parts of a whole** — a stacked bar for a handful of components, or a normalized stacked bar when shares matter more than totals. Avoid pie charts with many slices.
- **A single number** — a large labeled value, a progress bar, or a sentence. Do not dress one value up as a chart.

## Encode the data honestly

- Put labels at the marks or at the ends of lines; keep legends short and next to the data they describe.
- Start bar and area scales at zero. Do not crop a bar axis to make small differences look large.
- Avoid dual axes. They let two unrelated scales imply a relationship that may not exist. Use small multiples, an index to a shared baseline, or two separate charts instead.
- Avoid three-dimensional effects, perspective, shadows, and decorative gradients. They distort the positions and lengths readers compare.
- Sort categories by value or by a meaningful order (time, rank, size) and state which order you used.
- Keep gridlines and axis lines light and consistent. Remove visual elements that do not carry data.
- Show uncertainty where it exists: error bars, confidence bands, or a note about sample size and aggregation.
- Keep missing observations as gaps, not zeros. State the filters, grouping, and aggregation behind every total.

## Use color with purpose

- Let one accent color mark the series or point that matters. Keep the remaining series in neutral grays.
- Use qualitative hues only for a small set of categories, in a fixed order. Do not recycle hues when the series count grows.
- Use a sequential ramp for magnitude and a diverging ramp only when the data has a meaningful midpoint.
- Never rely on hue alone. Add direct labels, differing shapes, or line styles so the chart still reads in grayscale and for readers with color-vision differences.
- Aim for at least 3:1 contrast between graphical marks and their background, and at least 4.5:1 for text. Measure candidate colors with `scripts/check_contrast.py`; do not estimate by eye.
- Check both light and dark themes. A palette that works on a white background usually needs new values on a dark one.

## Build and verify

- Read the input first: confirm column names, units, date ranges, and record counts before plotting.
- Use the project's existing charting library and visual conventions. Add a dependency only when nothing available can do the job.
- Render the chart at the size it will be viewed. Look for clipped labels, overlapping ticks, unreadable small text, and misleading empty space.
- Compare the plotted values with the source data, then report the output path and the command that reproduces it.
- Work through `references/checklist.md` before calling the work finished. If you cannot inspect the rendered output, say so explicitly.
