-- The SAME text import is accepted by an ATTRIBUTE table: that is what they are for,
-- and attribute cubes never aggregate, so nothing can be corrupted.
CREATE ATTRIBUTE TABLE AttrText (Region STRING, Attribute STRING)
.import tests/scripts/import_attribute_text.csv AttrText
SELECT Region, value FROM AttrText