//! Stable display names for child runs.

use std::collections::HashSet;

/// Proper star names curated from the IAU Catalog of Star Names.
///
/// The order is intentional: familiar names are assigned first. These labels are
/// presentation metadata; run and attempt IDs remain the durable identities.
pub const STAR_NAMES: [&str; 100] = [
    "Sirius",
    "Canopus",
    "Arcturus",
    "Vega",
    "Capella",
    "Rigel",
    "Procyon",
    "Betelgeuse",
    "Achernar",
    "Hadar",
    "Altair",
    "Acrux",
    "Aldebaran",
    "Antares",
    "Spica",
    "Pollux",
    "Fomalhaut",
    "Deneb",
    "Mimosa",
    "Regulus",
    "Adhara",
    "Castor",
    "Gacrux",
    "Shaula",
    "Bellatrix",
    "Elnath",
    "Miaplacidus",
    "Alnilam",
    "Alnair",
    "Alioth",
    "Mirfak",
    "Dubhe",
    "Wezen",
    "Sargas",
    "Kaus Australis",
    "Avior",
    "Alkaid",
    "Menkalinan",
    "Atria",
    "Alhena",
    "Peacock",
    "Mirzam",
    "Alphard",
    "Hamal",
    "Diphda",
    "Nunki",
    "Menkent",
    "Alpheratz",
    "Kochab",
    "Saiph",
    "Denebola",
    "Rasalhague",
    "Algol",
    "Almach",
    "Caph",
    "Mizar",
    "Merak",
    "Izar",
    "Schedar",
    "Enif",
    "Ankaa",
    "Sabik",
    "Phecda",
    "Rasalgethi",
    "Unukalhai",
    "Scheat",
    "Markab",
    "Algieba",
    "Zubeneschamali",
    "Zubenelgenubi",
    "Aludra",
    "Gienah",
    "Zosma",
    "Chertan",
    "Cursa",
    "Ruchbah",
    "Algenib",
    "Vindemiatrix",
    "Sadr",
    "Mebsuta",
    "Pherkad",
    "Alcyone",
    "Atlas",
    "Electra",
    "Maia",
    "Merope",
    "Taygeta",
    "Pleione",
    "Celaeno",
    "Asterope",
    "Alnitak",
    "Mintaka",
    "Thuban",
    "Polaris",
    "Tarazed",
    "Alderamin",
    "Alshain",
    "Albireo",
    "Lesath",
    "Kornephoros",
];

/// Stable presentation name for one child in a wave. Waves are bounded far
/// below this catalogue; modulo keeps the helper total for defensive callers
/// without turning an absent display label into an execution failure.
pub(crate) fn at(index: usize) -> &'static str {
    STAR_NAMES[index % STAR_NAMES.len()]
}

pub(super) fn first_available<'a>(used: impl IntoIterator<Item = &'a str>) -> Option<&'static str> {
    let used: HashSet<&str> = used.into_iter().collect();
    STAR_NAMES.iter().copied().find(|name| !used.contains(name))
}

pub(super) fn contains(name: &str) -> bool {
    STAR_NAMES.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_has_one_hundred_case_insensitively_unique_names() {
        let unique: HashSet<String> = STAR_NAMES.iter().map(|name| name.to_lowercase()).collect();
        assert_eq!(STAR_NAMES.len(), 100);
        assert_eq!(unique.len(), STAR_NAMES.len());
    }

    #[test]
    fn allocation_uses_first_free_name_and_refuses_exhaustion() {
        assert_eq!(first_available(["Sirius", "Canopus"]), Some("Arcturus"));
        assert_eq!(first_available(STAR_NAMES), None);
    }

    #[test]
    fn wave_positions_use_the_same_familiar_names_as_the_desktop() {
        assert_eq!(at(0), "Sirius");
        assert_eq!(at(1), "Canopus");
        assert_eq!(at(2), "Arcturus");
    }
}
