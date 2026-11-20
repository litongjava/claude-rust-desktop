# Pre-publish checklist

Run this list against the rendered chart, not against the code alone.

## Question and form

- [ ] The chart answers one stated question.
- [ ] The chosen form fits the question: comparison, trend, distribution, relationship, or part-to-whole.
- [ ] A table, a large number, or a sentence would not communicate the result better.
- [ ] Categories are sorted by value or by an order that has a reason.

## Data

- [ ] Column names, units, and time zones match the source.
- [ ] The number of plotted observations matches the input after filtering.
- [ ] Missing values are shown as gaps or called out, never silently treated as zero.
- [ ] Aggregation, grouping, and filters are described next to the chart.
- [ ] Totals and percentages add up within rounding.

## Scale and marks

- [ ] Bars and areas start at zero.
- [ ] No dual-axis chart; if two measures share a chart, their scales are comparable.
- [ ] No 3D, perspective, shadows, or decorative gradients.
- [ ] Gridlines and axes are consistent and quieter than the data.
- [ ] Uncertainty is visible through error bars, bands, or a sample-size note.
- [ ] Long labels are wrapped, angled, or moved rather than clipped.

## Color

- [ ] One accent color highlights the important series; the rest are neutral.
- [ ] Categorical hues are used for identity, a sequential ramp for magnitude, and a diverging ramp only around a real midpoint.
- [ ] The chart remains readable in grayscale and with color-vision deficiencies.
- [ ] Graphic marks reach at least 3:1 contrast with their background.
- [ ] Text reaches at least 4.5:1 contrast with its background.
- [ ] Candidate colors were checked with `scripts/check_contrast.py`.
- [ ] Both light and dark themes were inspected.

## Rendering and delivery

- [ ] The rendered output was opened and inspected at the intended size.
- [ ] Labels, legends, ticks, and titles are legible and do not overlap.
- [ ] The title states what the chart shows; the subtitle or note gives the scope and date.
- [ ] A text summary or accessible table exists when the chart is the only delivery.
- [ ] The output path and the reproduction command are reported.
- [ ] Any part that could not be visually verified is named explicitly.
