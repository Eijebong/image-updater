use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Overrides {
    pub helm: HelmOverride,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct HelmOverride {
    pub parameters: ParametersOverride,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct ParametersOverride(pub Vec<Parameter>);

#[derive(Serialize, Deserialize, Debug)]
pub struct Parameter {
    pub name: String,
    pub value: String,
    pub forcestring: bool,
}
