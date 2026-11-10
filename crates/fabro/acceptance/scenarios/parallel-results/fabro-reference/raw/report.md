# Findings (2)

1. src/pager.py:12 (CONFIRMED): page_count drops the final partial page
   Failure scenario: 7 items with page size 3 reports 2 pages, so item 7 is unreachable
2. src/render.py:40 (PLAUSIBLE): render escapes the title twice
   Failure scenario: a title containing & renders as &amp;amp;
